export type AgentStatus = "idle" | "busy" | "offline" | "error";

export interface Agent {
  id: string;
  name: string;
  role: string;
  status: AgentStatus;
  lastActiveAt: string;
  tasksInFlight: number;
  capabilities: string[];
  metadata?: Record<string, unknown>;
}

export type TaskStatus =
  | "queued"
  | "running"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

export interface TaskSummary {
  id: string;
  agentId: string;
  title: string;
  status: TaskStatus;
  progress: number;
  queuedAt: string;
  startedAt?: string;
  finishedAt?: string;
  error?: string;
}

export interface InteractionEntry {
  id: string;
  agentId: string;
  taskId?: string;
  direction: "request" | "response" | "system";
  message: string;
  createdAt: string;
  metadata?: Record<string, unknown>;
}

export interface PagedResult<T> {
  items: T[];
  total: number;
  page: number;
  pageSize: number;
}

export type NotificationLevel = "info" | "success" | "warning" | "error";

export interface NotificationMessage {
  id: string;
  level: NotificationLevel;
  title: string;
  message: string;
  createdAt: number;
  meta?: Record<string, unknown>;
}
