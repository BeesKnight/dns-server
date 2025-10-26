import { PagedResult, TaskStatus, TaskSummary } from "../types/domain";
import { HttpRequest, HttpRequestOptions, buildQuery, request } from "./httpClient";

export interface TaskFilters {
  agentId?: string;
  status?: TaskStatus | "active";
  page?: number;
  pageSize?: number;
}

export interface TaskMutation {
  status?: TaskStatus;
  progress?: number;
  note?: string;
}

export const taskService = {
  list(filters: TaskFilters = {}, options?: HttpRequestOptions): HttpRequest<PagedResult<TaskSummary>> {
    const search = buildQuery({
      agentId: filters.agentId,
      status: filters.status,
      page: filters.page ?? 1,
      pageSize: filters.pageSize ?? 50,
    });

    return request<PagedResult<TaskSummary>>(`tasks?${search.toString()}`, {
      ...options,
      method: "get",
    });
  },

  update(id: string, mutation: TaskMutation, options?: HttpRequestOptions): HttpRequest<TaskSummary> {
    return request<TaskSummary>(`tasks/${id}`, {
      ...options,
      method: "patch",
      json: mutation,
    });
  },

  cancel(id: string, options?: HttpRequestOptions): HttpRequest<TaskSummary> {
    return request<TaskSummary>(`tasks/${id}/cancel`, {
      ...options,
      method: "post",
    });
  },
};
