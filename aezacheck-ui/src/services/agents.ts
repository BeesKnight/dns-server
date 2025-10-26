import { Agent, AgentStatus, PagedResult, TaskSummary } from "../types/domain";
import { HttpRequest, HttpRequestOptions, buildQuery, request } from "./httpClient";

export interface AgentListParams {
  status?: AgentStatus;
  search?: string;
  page?: number;
  pageSize?: number;
}

export interface AgentPayload {
  name: string;
  role: string;
  status?: AgentStatus;
  capabilities?: string[];
  metadata?: Record<string, unknown>;
}

export interface AgentPatch extends Partial<Omit<AgentPayload, "role">> {
  role?: string;
}

export const agentService = {
  list(params: AgentListParams = {}, options?: HttpRequestOptions): HttpRequest<PagedResult<Agent>> {
    const search = buildQuery({
      status: params.status,
      search: params.search,
      page: params.page ?? 1,
      pageSize: params.pageSize ?? 50,
    });
    return request<PagedResult<Agent>>(`v1/agents?${search.toString()}`, {
      ...options,
      method: "get",
    });
  },

  get(id: string, options?: HttpRequestOptions): HttpRequest<Agent> {
    return request<Agent>(`v1/agents/${id}`, { ...options, method: "get" });
  },

  create(payload: AgentPayload, options?: HttpRequestOptions): HttpRequest<Agent> {
    return request<Agent>("v1/agents", {
      ...options,
      method: "post",
      json: payload,
    });
  },

  update(id: string, patch: AgentPatch, options?: HttpRequestOptions): HttpRequest<Agent> {
    return request<Agent>(`v1/agents/${id}`, {
      ...options,
      method: "patch",
      json: patch,
    });
  },

  remove(id: string, options?: HttpRequestOptions): HttpRequest<{ id: string }> {
    return request<{ id: string }>(`v1/agents/${id}`, {
      ...options,
      method: "delete",
    });
  },

  tasks(
    id: string,
    params: { page?: number; pageSize?: number; status?: string } = {},
    options?: HttpRequestOptions
  ): HttpRequest<PagedResult<TaskSummary>> {
    const search = buildQuery({
      page: params.page ?? 1,
      pageSize: params.pageSize ?? 20,
      status: params.status,
    });
    return request<PagedResult<TaskSummary>>(`v1/agents/${id}/tasks?${search.toString()}`, {
      ...options,
      method: "get",
    });
  },
};
