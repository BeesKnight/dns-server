// src/lib/api.ts
export type User = {
  id: string;
  email: string;
  role: string;
  last_login_at?: string;
  current_ip?: string | null;
};

export type GeoInfo = {
  lat: number | null;
  lon: number | null;
  city: string | null;
  country: string | null;
  asn: number | null;
  asn_org: string | null;
};

/* ================= Helpers ================= */

const API_BASE = import.meta.env.VITE_API_BASE?.replace(/\/?$/, "") ?? (import.meta.env.DEV ? "/api" : "/v1");

import { request } from "../services";

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

  const { promise } = request<T>(path.replace(/^\//, ""), {
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
  const url = new URL(API_BASE + "/v1/map/events");
  const t = localStorage.getItem("access_token");
  if (t) url.searchParams.set("access_token", t);
  return new EventSource(url.toString());
}

/* ================= Public API ================= */

export const api = {
  // --- auth ---
  login: (email: string, password: string) =>
    call<{ access_token: string; expires_in: number; user: User }>("v1/auth/login", {
      method: "POST",
      body: { email, password },
    }),

  register: (email: string, password: string) =>
    call<{ access_token: string; expires_in: number; user: User }>("v1/auth/register", {
      method: "POST",
      body: { email, password },
    }),

  me: () =>
    call<User>("v1/auth/me"),

  // --- geo ---
  geoLookup: (ip: string) =>
    call<GeoInfo>(`v1/geo/lookup?ip=${encodeURIComponent(ip)}`),

  // --- checks ---
  startQuickCheck: (url: string) =>
    call<{ check_id: string }>("v1/jobs/checks", {
      method: "POST",
      body: { url, template: "quick" },
    }),

  startCheck: (url: string, kinds: string[], opts?: { dns_server?: string }) =>
    call<{ check_id: string }>("v1/jobs/checks", {
      method: "POST",
      body: { url, kinds, ...opts },
    }),
};
