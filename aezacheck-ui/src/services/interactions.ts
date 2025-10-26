import { InteractionEntry, PagedResult } from "../types/domain";
import { HttpRequest, HttpRequestOptions, buildQuery, request } from "./httpClient";

export interface InteractionFilters {
  agentId?: string;
  taskId?: string;
  page?: number;
  pageSize?: number;
}

export interface InteractionPayload {
  agentId: string;
  taskId?: string;
  direction: InteractionEntry["direction"];
  message: string;
  metadata?: Record<string, unknown>;
}

export const interactionService = {
  list(filters: InteractionFilters = {}, options?: HttpRequestOptions): HttpRequest<PagedResult<InteractionEntry>> {
    const search = buildQuery({
      agentId: filters.agentId,
      taskId: filters.taskId,
      page: filters.page ?? 1,
      pageSize: filters.pageSize ?? 50,
    });

    return request<PagedResult<InteractionEntry>>(`interactions?${search.toString()}`, {
      ...options,
      method: "get",
    });
  },

  create(payload: InteractionPayload, options?: HttpRequestOptions): HttpRequest<InteractionEntry> {
    return request<InteractionEntry>("interactions", {
      ...options,
      method: "post",
      json: payload,
    });
  },
};
