import { create } from "zustand";
import {
  Agent,
  AgentStatus,
  InteractionEntry,
  NotificationLevel,
  NotificationMessage,
  PagedResult,
  TaskSummary,
} from "../types/domain";
import {
  AgentListParams,
  AgentPatch,
  AgentPayload,
  InteractionFilters,
  InteractionPayload,
  agentService,
  interactionService,
  taskService,
  TaskFilters,
  TaskMutation,
  ApiError,
} from "../services";

type RequestMap = Record<string, AbortController>;

type Pagination = {
  page: number;
  pageSize: number;
  total: number;
};

type AgentStore = {
  agents: Agent[];
  tasksById: Record<string, TaskSummary>;
  interactions: InteractionEntry[];
  agentPage: Pagination;
  interactionPage: Pagination;
  taskPage: Pagination;
  loadingAgents: boolean;
  loadingTasks: boolean;
  loadingInteractions: boolean;
  notifications: NotificationMessage[];
  requestControllers: RequestMap;
  currentFilters: {
    agents?: AgentListParams;
    tasks?: TaskFilters;
    interactions?: InteractionFilters;
  };
  loadAgents: (params?: AgentListParams) => Promise<void>;
  loadTasks: (params?: TaskFilters) => Promise<void>;
  loadInteractions: (params?: InteractionFilters) => Promise<void>;
  createAgent: (payload: AgentPayload) => Promise<Agent>;
  updateAgent: (id: string, patch: AgentPatch) => Promise<Agent>;
  removeAgent: (id: string) => Promise<void>;
  mutateTask: (id: string, mutation: TaskMutation) => Promise<TaskSummary>;
  cancelTask: (id: string) => Promise<TaskSummary>;
  addInteraction: (payload: InteractionPayload) => Promise<InteractionEntry>;
  optimisticAgentStatus: (id: string, status: AgentStatus) => void;
  cancelRequest: (key: string) => void;
  dismissNotification: (id: string) => void;
  clearNotifications: () => void;
};

const defaultPagination = (): Pagination => ({ page: 1, pageSize: 50, total: 0 });

const safeId = () =>
  typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID()
    : `id-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;

const paginationFrom = <T>(payload: PagedResult<T>): Pagination => ({
  page: payload.page,
  pageSize: payload.pageSize,
  total: payload.total,
});

const deriveNotification = (
  level: NotificationLevel,
  message: string,
  meta?: Record<string, unknown>,
  title = "Ошибка"
): NotificationMessage => ({
  id: safeId(),
  level,
  title,
  message,
  createdAt: Date.now(),
  meta,
});

const removeController = (controllers: RequestMap, key: string): RequestMap => {
  if (!controllers[key]) return controllers;
  const clone = { ...controllers };
  delete clone[key];
  return clone;
};

const registerController = (controllers: RequestMap, key: string, controller: AbortController): RequestMap => ({
  ...controllers,
  [key]: controller,
});

export const useAgentStore = create<AgentStore>((set, get) => ({
  agents: [],
  tasksById: {},
  interactions: [],
  agentPage: defaultPagination(),
  interactionPage: defaultPagination(),
  taskPage: defaultPagination(),
  loadingAgents: false,
  loadingTasks: false,
  loadingInteractions: false,
  notifications: [],
  requestControllers: {},
  currentFilters: {},

  async loadAgents(params) {
    const filters = { ...params };
    const key = `agents:${JSON.stringify(filters)}`;
    get().cancelRequest(key);
    const { promise, controller } = agentService.list(filters);
    set((state) => ({
      loadingAgents: true,
      currentFilters: { ...state.currentFilters, agents: filters },
      requestControllers: registerController(state.requestControllers, key, controller),
    }));
    try {
      const data = await promise;
      set((state) => ({
        agents: data.items,
        agentPage: paginationFrom(data),
        loadingAgents: false,
        requestControllers: removeController(state.requestControllers, key),
      }));
    } catch (error) {
      set((state) => ({
        loadingAgents: false,
        requestControllers: removeController(state.requestControllers, key),
        notifications: state.notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось загрузить агентов",
            { scope: "agents", filters }
          )
        ),
      }));
      throw error;
    }
  },

  async loadTasks(params) {
    const filters = { ...params };
    const key = `tasks:${JSON.stringify(filters)}`;
    get().cancelRequest(key);
    const { promise, controller } = taskService.list(filters);
    set((state) => ({
      loadingTasks: true,
      currentFilters: { ...state.currentFilters, tasks: filters },
      requestControllers: registerController(state.requestControllers, key, controller),
    }));
    try {
      const data = await promise;
      const tasksById = data.items.reduce<Record<string, TaskSummary>>((acc, task) => {
        acc[task.id] = task;
        return acc;
      }, {});
      set((state) => ({
        tasksById: { ...state.tasksById, ...tasksById },
        taskPage: paginationFrom(data),
        loadingTasks: false,
        requestControllers: removeController(state.requestControllers, key),
      }));
    } catch (error) {
      set((state) => ({
        loadingTasks: false,
        requestControllers: removeController(state.requestControllers, key),
        notifications: state.notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось загрузить задачи",
            { scope: "tasks", filters }
          )
        ),
      }));
      throw error;
    }
  },

  async loadInteractions(params) {
    const filters = { ...params };
    const key = `interactions:${JSON.stringify(filters)}`;
    get().cancelRequest(key);
    const { promise, controller } = interactionService.list(filters);
    set((state) => ({
      loadingInteractions: true,
      currentFilters: { ...state.currentFilters, interactions: filters },
      requestControllers: registerController(state.requestControllers, key, controller),
    }));
    try {
      const data = await promise;
      set((state) => ({
        interactions:
          data.page > 1 ? state.interactions.concat(data.items) : data.items,
        interactionPage: paginationFrom(data),
        loadingInteractions: false,
        requestControllers: removeController(state.requestControllers, key),
      }));
    } catch (error) {
      set((state) => ({
        loadingInteractions: false,
        requestControllers: removeController(state.requestControllers, key),
        notifications: state.notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось загрузить историю",
            { scope: "interactions", filters }
          )
        ),
      }));
      throw error;
    }
  },

  async createAgent(payload) {
    const tempId = safeId();
    const optimisticAgent: Agent = {
      id: tempId,
      tasksInFlight: 0,
      lastActiveAt: new Date().toISOString(),
      status: payload.status ?? "idle",
      metadata: payload.metadata,
      capabilities: payload.capabilities ?? [],
      name: payload.name,
      role: payload.role,
    };

    set((state) => ({ agents: [optimisticAgent, ...state.agents] }));

    try {
      const { promise } = agentService.create(payload);
      const result = await promise;
      set((state) => ({
        agents: state.agents.map((agent) => (agent.id === tempId ? result : agent)),
      }));
      return result;
    } catch (error) {
      set((state) => ({
        agents: state.agents.filter((agent) => agent.id !== tempId),
        notifications: state.notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось создать агента",
            { scope: "agents", payload }
          )
        ),
      }));
      throw error;
    }
  },

  async updateAgent(id, patch) {
    const previous = get().agents;
    set({
      agents: previous.map((agent) => (agent.id === id ? { ...agent, ...patch } : agent)),
    });

    try {
      const { promise } = agentService.update(id, patch);
      const updated = await promise;
      set((state) => ({
        agents: state.agents.map((agent) => (agent.id === id ? updated : agent)),
      }));
      return updated;
    } catch (error) {
      set({
        agents: previous,
        notifications: get().notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось обновить агента",
            { scope: "agents", id, patch }
          )
        ),
      });
      throw error;
    }
  },

  async removeAgent(id) {
    const previous = get().agents;
    set({ agents: previous.filter((agent) => agent.id !== id) });
    try {
      const { promise } = agentService.remove(id);
      await promise;
    } catch (error) {
      set({
        agents: previous,
        notifications: get().notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось удалить агента",
            { scope: "agents", id }
          )
        ),
      });
      throw error;
    }
  },

  async mutateTask(id, mutation) {
    const previous = get().tasksById[id];
    if (previous) {
      set((state) => ({
        tasksById: {
          ...state.tasksById,
          [id]: { ...previous, ...mutation, progress: mutation.progress ?? previous.progress },
        },
      }));
    }
    try {
      const { promise } = taskService.update(id, mutation);
      const updated = await promise;
      set((state) => ({
        tasksById: { ...state.tasksById, [id]: updated },
      }));
      return updated;
    } catch (error) {
      if (previous) {
        set((state) => ({
          tasksById: { ...state.tasksById, [id]: previous },
          notifications: state.notifications.concat(
            deriveNotification(
              "error",
              error instanceof ApiError ? error.message : "Не удалось обновить задачу",
              { scope: "tasks", id, mutation }
            )
          ),
        }));
      }
      throw error;
    }
  },

  async cancelTask(id) {
    const { promise } = taskService.cancel(id);
    try {
      const task = await promise;
      set((state) => ({ tasksById: { ...state.tasksById, [id]: task } }));
      return task;
    } catch (error) {
      set((state) => ({
        notifications: state.notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось отменить задачу",
            { scope: "tasks", id }
          )
        ),
      }));
      throw error;
    }
  },

  async addInteraction(payload) {
    const optimistic: InteractionEntry = {
      id: safeId(),
      createdAt: new Date().toISOString(),
      metadata: payload.metadata,
      taskId: payload.taskId,
      agentId: payload.agentId,
      direction: payload.direction,
      message: payload.message,
    };
    set((state) => ({ interactions: [optimistic, ...state.interactions] }));
    try {
      const { promise } = interactionService.create(payload);
      const created = await promise;
      set((state) => ({
        interactions: state.interactions.map((entry) => (entry.id === optimistic.id ? created : entry)),
      }));
      return created;
    } catch (error) {
      set((state) => ({
        interactions: state.interactions.filter((entry) => entry.id !== optimistic.id),
        notifications: state.notifications.concat(
          deriveNotification(
            "error",
            error instanceof ApiError ? error.message : "Не удалось сохранить сообщение",
            { scope: "interactions", payload }
          )
        ),
      }));
      throw error;
    }
  },

  optimisticAgentStatus(id, status) {
    set((state) => ({
      agents: state.agents.map((agent) =>
        agent.id === id ? { ...agent, status, lastActiveAt: new Date().toISOString() } : agent
      ),
    }));
  },

  cancelRequest(key) {
    const controller = get().requestControllers[key];
    if (controller) {
      controller.abort();
      set((state) => ({ requestControllers: removeController(state.requestControllers, key) }));
    }
  },

  dismissNotification(id) {
    set((state) => ({ notifications: state.notifications.filter((note) => note.id !== id) }));
  },

  clearNotifications() {
    set({ notifications: [] });
  },
}));
