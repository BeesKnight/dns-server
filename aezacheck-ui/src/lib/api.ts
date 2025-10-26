// src/lib/api.ts
export type User = {
  id: string;
  email: string;
  role: string;
  last_login_at?: string;
  current_ip?: string | null;
};

/* ================= Helpers ================= */

import { request, resolveApiBase } from "../services";

const API_BASE = resolveApiBase();

const ensureTrailingSlash = (value: string) => (value.endsWith("/") ? value : `${value}/`);
const trimLeadingSlash = (value: string) => value.replace(/^\/+/u, "");
const isAbsoluteUrl = (value: string) => /^[a-zA-Z][a-zA-Z\d+\-.]*:/u.test(value);

const baseForJoin = isAbsoluteUrl(API_BASE)
  ? new URL(ensureTrailingSlash(API_BASE))
  : new URL(ensureTrailingSlash(API_BASE), "http://placeholder/");

const resolveRelativePath = (path: string): string => {
  const target = trimLeadingSlash(path);
  const joined = new URL(target || ".", baseForJoin);
  const basePath = baseForJoin.pathname.endsWith("/") ? baseForJoin.pathname : ensureTrailingSlash(baseForJoin.pathname);
  const relative = joined.pathname.startsWith(basePath)
    ? joined.pathname.slice(basePath.length)
    : joined.pathname;
  return trimLeadingSlash(relative);
};

const resolveAbsoluteBase = (): string => {
  if (isAbsoluteUrl(API_BASE)) return ensureTrailingSlash(API_BASE);
  if (typeof window === "undefined") {
    throw new Error("Cannot resolve absolute API base outside of a browser environment");
  }
  return new URL(ensureTrailingSlash(API_BASE), window.location.origin).toString();
};

type CallInit = Omit<RequestInit, "body"> & {
  method?: string;
  body?: unknown;
};

const call = async <T>(path: string, init: CallInit = {}): Promise<T> => {
  let payload = init.body;
  if (typeof payload === "string") {
    try {
      payload = JSON.parse(payload);
    } catch {
      payload = { raw: payload };
    }
  }

  const endpoint = resolveRelativePath(path);
  const { promise } = request<T>(endpoint, {
    method: (init.method as never) ?? "get",
    json: payload,
  });
  return promise;
};

/**
 * SSE-подключение для карты.
 * EventSource не поддерживает кастомные заголовки,
 * поэтому при необходимости передаём токен в query.
 */
export function mapSSE(): EventSource {
  const url = new URL("map/events", resolveAbsoluteBase());
  const t = localStorage.getItem("access_token");
  if (t) url.searchParams.set("access_token", t);
  return new EventSource(url.toString());
}

/* ================= Public API ================= */

export const api = {
  // --- auth ---
  login: (email: string, password: string) =>
    call<{ access_token: string; expires_in: number; user: User }>("auth/login", {
      method: "POST",
      body: { email, password },
    }),

  register: (email: string, password: string) =>
    call<{ access_token: string; expires_in: number; user: User }>("auth/register", {
      method: "POST",
      body: { email, password },
    }),

  me: () =>
    call<User>("auth/me"),

  // --- checks ---
  startQuickCheck: (url: string) =>
    call<{ check_id: string }>("jobs/checks", {
      method: "POST",
      body: { url, template: "quick" },
    }),

  startCheck: (url: string, kinds: string[], opts?: { dns_server?: string }) =>
    call<{ check_id: string }>("jobs/checks", {
      method: "POST",
      body: { url, kinds, ...opts },
    }),
};
