import { useCallback, useEffect, useMemo, useState } from "react";
import type { Agent, AgentStatus, InteractionEntry, TaskSummary, TaskStatus } from "../types/domain";
import { useAgentStore } from "../store/agents";
import { VirtualizedList } from "./agent-dashboard/VirtualizedList";
import type { AgentListParams } from "../services/agents";

const agentStatusMeta: Record<AgentStatus, { label: string; color: string }> = {
  idle: { label: "Свободен", color: "bg-emerald-500/20 text-emerald-200" },
  busy: { label: "Занят", color: "bg-blue-500/20 text-blue-200" },
  offline: { label: "Отключён", color: "bg-slate-500/20 text-slate-200" },
  error: { label: "Ошибка", color: "bg-rose-500/20 text-rose-200" },
};

const taskStatusOrder: TaskStatus[] = ["queued", "running", "paused", "completed", "failed", "cancelled"];

const taskStatusLabels: Record<TaskStatus, string> = {
  queued: "В очереди",
  running: "Выполняется",
  paused: "На паузе",
  completed: "Завершена",
  failed: "Ошибка",
  cancelled: "Отменена",
};

const formatPercent = (value: number) => `${Math.min(Math.max(value, 0), 100).toFixed(0)}%`;

const relativeFormatter = new Intl.RelativeTimeFormat("ru", { numeric: "auto" });

const formatRelative = (dateString: string | undefined) => {
  if (!dateString) return "–";
  const date = new Date(dateString);
  const diffMs = date.getTime() - Date.now();
  const minutes = Math.round(diffMs / (60 * 1000));
  if (Math.abs(minutes) < 60) return relativeFormatter.format(minutes, "minute");
  const hours = Math.round(diffMs / (60 * 60 * 1000));
  if (Math.abs(hours) < 24) return relativeFormatter.format(hours, "hour");
  const days = Math.round(diffMs / (24 * 60 * 60 * 1000));
  return relativeFormatter.format(days, "day");
};

type AgentFilters = {
  search: string;
  status: AgentStatus | "all";
  pageSize: number;
};

const sanitizeAgentFilters = (filters: AgentFilters, page: number): AgentListParams => ({
  page,
  ...(filters.search ? { search: filters.search } : {}),
  ...(filters.status !== "all" ? { status: filters.status } : {}),
  pageSize: filters.pageSize,
});

const statusOptions: (AgentStatus | "all")[] = ["all", "idle", "busy", "offline", "error"];

const statusLabel = (value: AgentStatus | "all") =>
  value === "all" ? "Все агенты" : agentStatusMeta[value].label;

const listSkeleton = Array.from({ length: 8 }, (_, index) => (
  <div
    key={index}
    className="animate-pulse rounded-xl border border-white/5 bg-white/5 p-4"
    style={{ height: 72 }}
  />
));

const timelineSkeleton = Array.from({ length: 6 }, (_, index) => (
  <div
    key={index}
    className="animate-pulse rounded-lg border border-white/5 bg-white/5 p-3"
    style={{ height: 60 }}
  />
));

const sumByStatus = (tasks: TaskSummary[]): Record<TaskStatus, TaskSummary[]> => {
  return tasks.reduce<Record<TaskStatus, TaskSummary[]>>((acc, task) => {
    if (!acc[task.status]) acc[task.status] = [];
    acc[task.status].push(task);
    return acc;
  }, {
    queued: [],
    running: [],
    paused: [],
    completed: [],
    failed: [],
    cancelled: [],
  });
};

const directionLabel: Record<InteractionEntry["direction"], string> = {
  request: "Запрос",
  response: "Ответ",
  system: "Система",
};

export function AgentDashboard() {
  const agents = useAgentStore((state) => state.agents);
  const agentPage = useAgentStore((state) => state.agentPage);
  const loadingAgents = useAgentStore((state) => state.loadingAgents);
  const notifications = useAgentStore((state) => state.notifications);
  const dismissNotification = useAgentStore((state) => state.dismissNotification);
  const loadAgents = useAgentStore((state) => state.loadAgents);
  const updateAgent = useAgentStore((state) => state.updateAgent);
  const optimisticAgentStatus = useAgentStore((state) => state.optimisticAgentStatus);
  const cancelRequest = useAgentStore((state) => state.cancelRequest);
  const loadTasks = useAgentStore((state) => state.loadTasks);
  const tasksById = useAgentStore((state) => state.tasksById);
  const loadingTasks = useAgentStore((state) => state.loadingTasks);
  const loadInteractions = useAgentStore((state) => state.loadInteractions);
  const interactions = useAgentStore((state) => state.interactions);
  const interactionPage = useAgentStore((state) => state.interactionPage);
  const loadingInteractions = useAgentStore((state) => state.loadingInteractions);
  const clearNotifications = useAgentStore((state) => state.clearNotifications);

  const [filters, setFilters] = useState<AgentFilters>({ search: "", status: "all", pageSize: 100 });
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [activeAgentsRequestKey, setActiveAgentsRequestKey] = useState<string | null>(null);

  const runLoadAgents = useCallback(
    async (page = 1) => {
      const sanitized = sanitizeAgentFilters(filters, page);
      const key = `agents:${JSON.stringify(sanitized)}`;
      setActiveAgentsRequestKey(key);
      try {
        await loadAgents(sanitized);
      } catch (error) {
        console.error(error);
      } finally {
        setActiveAgentsRequestKey(null);
      }
    },
    [filters, loadAgents]
  );

  useEffect(() => {
    const timer = window.setTimeout(() => {
      void runLoadAgents(1);
    }, 250);
    return () => window.clearTimeout(timer);
  }, [filters, runLoadAgents]);

  useEffect(() => {
    if (!selectedAgentId && agents.length > 0) {
      setSelectedAgentId(agents[0].id);
    }
  }, [agents, selectedAgentId]);

  useEffect(() => {
    if (!selectedAgentId) return;
    void loadTasks({ agentId: selectedAgentId, page: 1, pageSize: 50 });
    void loadInteractions({ agentId: selectedAgentId, page: 1, pageSize: 50 });
  }, [loadInteractions, loadTasks, selectedAgentId]);

  const selectedAgent = useMemo(
    () => agents.find((agent) => agent.id === selectedAgentId) ?? null,
    [agents, selectedAgentId]
  );

  const agentTasks = useMemo(() => {
    if (!selectedAgentId) return [] as TaskSummary[];
    return Object.values(tasksById).filter((task) => task.agentId === selectedAgentId);
  }, [selectedAgentId, tasksById]);

  const aggregatedTasks = useMemo(() => sumByStatus(agentTasks), [agentTasks]);

  const agentInteractions = useMemo(() => interactions.filter((item) => item.agentId === selectedAgentId), [
    interactions,
    selectedAgentId,
  ]);

  const handleStatusChange = async (agent: Agent, status: AgentStatus) => {
    optimisticAgentStatus(agent.id, status);
    try {
      await updateAgent(agent.id, { status });
    } catch (error) {
      console.error(error);
    }
  };

  const handleLoadMoreInteractions = () => {
    if (!selectedAgentId) return;
    const nextPage = interactionPage.page + 1;
    if (agentInteractions.length >= interactionPage.total) return;
    void loadInteractions({ agentId: selectedAgentId, page: nextPage, pageSize: interactionPage.pageSize });
  };

  const totalAgents = agentPage.total;

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 rounded-2xl border border-white/10 bg-slate-900/60 p-4 backdrop-blur">
        <div className="flex flex-wrap items-center gap-3">
          <h2 className="text-xl font-semibold text-white">Агенты</h2>
          <span className="rounded-full bg-white/10 px-3 py-1 text-sm text-slate-200">{totalAgents} всего</span>
          <div className="flex items-center gap-2 text-sm text-slate-300">
            <button
              type="button"
              onClick={() => void runLoadAgents(agentPage.page)}
              className="rounded-lg border border-white/10 px-3 py-1.5 text-sm font-medium text-slate-100 transition hover:bg-white/10"
            >
              Обновить
            </button>
            <button
              type="button"
              disabled={!activeAgentsRequestKey}
              onClick={() => {
                if (!activeAgentsRequestKey) return;
                cancelRequest(activeAgentsRequestKey);
                setActiveAgentsRequestKey(null);
              }}
              className="rounded-lg border border-white/10 px-3 py-1.5 text-sm font-medium text-slate-100 transition hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-40"
            >
              Отменить запрос
            </button>
            <button
              type="button"
              onClick={() => clearNotifications()}
              className="rounded-lg border border-white/10 px-3 py-1.5 text-sm font-medium text-slate-100 transition hover:bg-white/10"
            >
              Очистить уведомления
            </button>
          </div>
        </div>

        <div className="flex flex-wrap gap-3">
          <input
            className="w-full min-w-[200px] flex-1 rounded-xl border border-white/10 bg-white/5 px-4 py-2 text-sm text-white placeholder:text-slate-300 focus:outline-none focus:ring-2 focus:ring-sky-500/50"
            placeholder="Поиск по имени или роли"
            value={filters.search}
            onChange={(event) =>
              setFilters((prev) => ({ ...prev, search: event.target.value }))
            }
          />
          <select
            className="rounded-xl border border-white/10 bg-white/5 px-3 py-2 text-sm text-white focus:outline-none focus:ring-2 focus:ring-sky-500/50"
            value={filters.status}
            onChange={(event) =>
              setFilters((prev) => ({ ...prev, status: event.target.value as AgentStatus | "all" }))
            }
          >
            {statusOptions.map((option) => (
              <option key={option} value={option} className="text-slate-900">
                {statusLabel(option)}
              </option>
            ))}
          </select>
          <select
            className="rounded-xl border border-white/10 bg-white/5 px-3 py-2 text-sm text-white focus:outline-none focus:ring-2 focus:ring-sky-500/50"
            value={filters.pageSize}
            onChange={(event) =>
              setFilters((prev) => ({ ...prev, pageSize: Number(event.target.value) }))
            }
          >
            {[50, 100, 200].map((size) => (
              <option key={size} value={size} className="text-slate-900">
                {size} на странице
              </option>
            ))}
          </select>
        </div>
      </header>

      <div className="grid gap-6 lg:grid-cols-5">
        <section className="lg:col-span-2 space-y-3">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-white">Список агентов</h3>
            <div className="flex items-center gap-2 text-xs text-slate-300">
              <button
                type="button"
                disabled={agentPage.page <= 1}
                onClick={() => void runLoadAgents(Math.max(agentPage.page - 1, 1))}
                className="rounded-lg border border-white/10 px-2 py-1 transition hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-40"
              >
                Назад
              </button>
              <span>
                {agentPage.page} / {Math.max(1, Math.ceil(agentPage.total / filters.pageSize))}
              </span>
              <button
                type="button"
                disabled={agentPage.page >= Math.ceil(agentPage.total / filters.pageSize)}
                onClick={() => void runLoadAgents(agentPage.page + 1)}
                className="rounded-lg border border-white/10 px-2 py-1 transition hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-40"
              >
                Вперёд
              </button>
            </div>
          </div>
          <div className="max-h-[520px] overflow-hidden rounded-2xl border border-white/10 bg-slate-900/60 p-2">
            {loadingAgents ? (
              <div className="space-y-2">{listSkeleton}</div>
            ) : agents.length > 0 ? (
              <VirtualizedList
                className="max-h-[512px] overflow-y-auto pr-1"
                innerClassName="space-y-2"
                itemHeight={80}
                items={agents}
                keyExtractor={(agent) => agent.id}
                renderItem={(agent) => (
                  <button
                    type="button"
                    onClick={() => setSelectedAgentId(agent.id)}
                    className={`w-full rounded-xl border px-4 py-3 text-left transition hover:border-sky-400/60 hover:bg-sky-500/10 ${
                      selectedAgentId === agent.id
                        ? "border-sky-400/80 bg-sky-500/10"
                        : "border-white/5 bg-white/5"
                    }`}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div>
                        <div className="text-sm font-semibold text-white">{agent.name}</div>
                        <div className="text-xs text-slate-300">{agent.role}</div>
                      </div>
                      <span
                        className={`rounded-full px-3 py-1 text-xs font-medium ${agentStatusMeta[agent.status].color}`}
                      >
                        {agentStatusMeta[agent.status].label}
                      </span>
                    </div>
                    <div className="mt-2 flex flex-wrap items-center gap-3 text-xs text-slate-300">
                      <span>Активность: {formatRelative(agent.lastActiveAt)}</span>
                      <span>Задач в работе: {agent.tasksInFlight}</span>
                      <span className="flex items-center gap-1">
                        <select
                          className="rounded-md border border-white/10 bg-white/5 px-2 py-1 text-xs text-white focus:outline-none"
                          value={agent.status}
                          onClick={(event) => event.stopPropagation()}
                          onChange={(event) => {
                            event.stopPropagation();
                            void handleStatusChange(agent, event.target.value as AgentStatus);
                          }}
                        >
                          {statusOptions
                            .filter((status): status is AgentStatus => status !== "all")
                            .map((status) => (
                              <option key={status} value={status} className="text-slate-900">
                                {agentStatusMeta[status].label}
                              </option>
                            ))}
                        </select>
                      </span>
                    </div>
                  </button>
                )}
                emptyState={<div className="p-6 text-center text-sm text-slate-300">Нет агентов</div>}
              />
            ) : (
              <div className="p-6 text-center text-sm text-slate-300">Нет агентов</div>
            )}
          </div>
        </section>

        <section className="lg:col-span-3 space-y-6">
          <div className="rounded-2xl border border-white/10 bg-slate-900/60 p-4">
            <div className="flex items-center justify-between">
              <h3 className="text-lg font-semibold text-white">Статусы задач</h3>
              {selectedAgent && (
                <div className="text-sm text-slate-300">{selectedAgent.name}</div>
              )}
            </div>
            {loadingTasks ? (
              <div className="mt-4 grid gap-3 md:grid-cols-3">{listSkeleton.slice(0, 3)}</div>
            ) : (
              <div className="mt-4 grid gap-3 md:grid-cols-3">
                {taskStatusOrder.map((status) => {
                  const tasks = aggregatedTasks[status] ?? [];
                  const avgProgress = tasks.length
                    ? tasks.reduce((sum, task) => sum + (task.progress ?? 0), 0) / tasks.length
                    : 0;
                  return (
                    <div key={status} className="rounded-xl border border-white/10 bg-white/5 p-4">
                      <div className="text-sm font-semibold text-white">{taskStatusLabels[status]}</div>
                      <div className="mt-2 text-3xl font-bold text-slate-100">{tasks.length}</div>
                      <div className="mt-3 text-xs text-slate-300">Средний прогресс</div>
                      <div className="mt-1 h-2 w-full rounded-full bg-white/5">
                        <div
                          className="h-full rounded-full bg-gradient-to-r from-sky-400 to-cyan-400"
                          style={{ width: formatPercent(avgProgress) }}
                        />
                      </div>
                      <div className="mt-2 text-xs text-slate-400">{formatPercent(avgProgress)}</div>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          <div className="rounded-2xl border border-white/10 bg-slate-900/60 p-4">
            <div className="flex items-center justify-between">
              <h3 className="text-lg font-semibold text-white">История взаимодействий</h3>
              <div className="text-xs text-slate-300">
                {agentInteractions.length} из {interactionPage.total}
              </div>
            </div>
            <div className="mt-4 max-h-[420px] overflow-hidden">
              {loadingInteractions && agentInteractions.length === 0 ? (
                <div className="space-y-2">{timelineSkeleton}</div>
              ) : agentInteractions.length > 0 ? (
                <VirtualizedList
                  className="max-h-[400px] overflow-y-auto pr-1"
                  innerClassName="space-y-2"
                  items={agentInteractions}
                  itemHeight={80}
                  keyExtractor={(item) => item.id}
                  renderItem={(entry) => (
                    <div className="rounded-xl border border-white/10 bg-white/5 p-3">
                      <div className="flex items-center justify-between text-xs text-slate-300">
                        <span className="font-semibold text-slate-100">{directionLabel[entry.direction]}</span>
                        <span>{new Date(entry.createdAt).toLocaleString()}</span>
                      </div>
                      <div className="mt-2 text-sm text-slate-100">{entry.message}</div>
                    </div>
                  )}
                />
              ) : (
                <div className="p-4 text-center text-sm text-slate-300">Нет взаимодействий</div>
              )}
            </div>
            <div className="mt-4 flex justify-between text-xs text-slate-300">
              <button
                type="button"
                onClick={handleLoadMoreInteractions}
                disabled={agentInteractions.length >= interactionPage.total || loadingInteractions}
                className="rounded-lg border border-white/10 px-3 py-1.5 transition hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-40"
              >
                Загрузить ещё
              </button>
              <button
                type="button"
                onClick={() =>
                  selectedAgentId &&
                  void loadInteractions({ agentId: selectedAgentId, page: 1, pageSize: interactionPage.pageSize })
                }
                className="rounded-lg border border-white/10 px-3 py-1.5 transition hover:bg-white/10"
              >
                Обновить
              </button>
            </div>
          </div>
        </section>
      </div>

      {notifications.length > 0 && (
        <aside className="space-y-2">
          {notifications.map((notification) => (
            <div
              key={notification.id}
              className="flex items-start justify-between gap-3 rounded-xl border border-rose-500/40 bg-rose-500/10 p-3 text-sm text-rose-100"
            >
              <div>
                <div className="font-semibold">{notification.title}</div>
                <div>{notification.message}</div>
              </div>
              <button
                type="button"
                className="rounded-md border border-white/10 px-2 py-1 text-xs text-white hover:bg-white/10"
                onClick={() => dismissNotification(notification.id)}
              >
                Закрыть
              </button>
            </div>
          ))}
        </aside>
      )}
    </div>
  );
}

export default AgentDashboard;
