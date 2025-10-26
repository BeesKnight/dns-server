import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Agent, InteractionEntry } from "../types/domain";
import { useAgentStore } from "./agents";
import { agentService, interactionService, ApiError } from "../services";

function createHttpRequest<T>(value: Promise<T>) {
  const controller = new AbortController();
  return {
    promise: value,
    controller,
    abort: controller.abort.bind(controller),
  };
}

vi.mock("../services", () => {
  class ApiError extends Error {}
  const agentService = {
    list: vi.fn(),
    create: vi.fn(),
    update: vi.fn(),
    remove: vi.fn(),
    tasks: vi.fn(),
  };
  const taskService = {
    list: vi.fn(),
    update: vi.fn(),
    cancel: vi.fn(),
  };
  const interactionService = {
    list: vi.fn(),
    create: vi.fn(),
  };
  return {
    ApiError,
    agentService,
    taskService,
    interactionService,
  };
});

const agent: Agent = {
  id: "agent-1",
  name: "Agent One",
  role: "resolver",
  status: "idle",
  lastActiveAt: new Date().toISOString(),
  tasksInFlight: 1,
  capabilities: ["dns"],
};

const interaction: InteractionEntry = {
  id: "interaction-1",
  agentId: "agent-1",
  taskId: "task-1",
  direction: "response",
  message: "Done",
  createdAt: new Date().toISOString(),
};

const mockedAgentService = agentService as unknown as {
  list: ReturnType<typeof vi.fn>;
  create: ReturnType<typeof vi.fn>;
  update: ReturnType<typeof vi.fn>;
  remove: ReturnType<typeof vi.fn>;
};

const mockedInteractionService = interactionService as unknown as {
  list: ReturnType<typeof vi.fn>;
  create: ReturnType<typeof vi.fn>;
};

const resetStore = () => {
  const state = useAgentStore.getState();
  useAgentStore.setState({
    ...state,
    agents: [],
    tasksById: {},
    interactions: [],
    agentPage: { page: 1, pageSize: 50, total: 0 },
    interactionPage: { page: 1, pageSize: 50, total: 0 },
    taskPage: { page: 1, pageSize: 50, total: 0 },
    loadingAgents: false,
    loadingTasks: false,
    loadingInteractions: false,
    notifications: [],
    requestControllers: {},
    currentFilters: {},
  });
};

beforeEach(() => {
  resetStore();
  vi.clearAllMocks();
});

describe("useAgentStore", () => {
  it("loads agents and updates pagination", async () => {
    mockedAgentService.list.mockReturnValue(
      createHttpRequest(
        Promise.resolve({ items: [agent], page: 1, pageSize: 50, total: 1 })
      )
    );

    await useAgentStore.getState().loadAgents();

    const state = useAgentStore.getState();
    expect(state.agents).toHaveLength(1);
    expect(state.agentPage.total).toBe(1);
  });

  it("reverts optimistic update on failure", async () => {
    useAgentStore.setState((state) => ({ ...state, agents: [agent] }));

    mockedAgentService.update.mockReturnValue(
      createHttpRequest(Promise.reject(new ApiError("Ошибка")))
    );

    await expect(
      useAgentStore.getState().updateAgent("agent-1", { status: "busy" })
    ).rejects.toThrow();

    const state = useAgentStore.getState();
    expect(state.agents[0].status).toBe("idle");
    expect(state.notifications).toHaveLength(1);
  });

  it("appends interactions with pagination", async () => {
    mockedInteractionService.list
      .mockReturnValueOnce(
        createHttpRequest(
          Promise.resolve({ items: [interaction], page: 1, pageSize: 50, total: 2 })
        )
      )
      .mockReturnValueOnce(
        createHttpRequest(
          Promise.resolve({
            items: [
              { ...interaction, id: "interaction-2", message: "Next" },
            ],
            page: 2,
            pageSize: 50,
            total: 2,
          })
        )
      );

    await useAgentStore.getState().loadInteractions({ agentId: "agent-1" });
    await useAgentStore.getState().loadInteractions({ agentId: "agent-1", page: 2 });

    const state = useAgentStore.getState();
    expect(state.interactions).toHaveLength(2);
    expect(state.interactions[1].message).toBe("Next");
  });
});
