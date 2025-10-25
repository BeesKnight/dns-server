// src/lib/api.ts
const API_BASE = import.meta.env.VITE_API_BASE || "http://localhost:8088";

export type User = {
  id: string;
  email: string;
  role: string;
  last_login_at?: string;
  current_ip?: string | null;
};

/* ================= Helpers ================= */

function authHeader(): Record<string, string> {
  const t = localStorage.getItem("access_token");
  return t ? { Authorization: `Bearer ${t}` } : {};
}

const jsonAuthHeaders = (): HeadersInit => ({
  "Content-Type": "application/json",
  ...authHeader(),
});

/**
 * Универсальный fetch-обёртка:
 * - читает тело как текст один раз
 * - пытается парсить JSON даже при `text/plain`
 * - если тело пустое — возвращает undefined
 */
async function req<T = unknown>(path: string, init: RequestInit = {}): Promise<T> {
  const r = await fetch(API_BASE + path, init);

  if (!r.ok) {
    const msg = await r.text().catch(() => "");
    throw new Error(msg || `HTTP ${r.status}`);
  }

  const text = await r.text().catch(() => "");

  if (!text) {
    // @ts-expect-error вызывающий может ожидать void
    return undefined;
  }

  try {
    return JSON.parse(text) as T;
  } catch {
    // не JSON — вернём строку как есть
    return text as unknown as T;
  }
}

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
    req<{ access_token: string; expires_in: number; user: User }>("/v1/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email, password }),
    }),

  register: (email: string, password: string) =>
    req<{ access_token: string; expires_in: number; user: User }>("/v1/auth/register", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email, password }),
    }),

  me: () =>
    req<User>("/v1/auth/me", {
      headers: authHeader(),
    }),

  // --- checks ---
startQuickCheck: (url: string) =>
  req<{ check_id: string }>("/v1/jobs/checks", {
    method: "POST",
    headers: jsonAuthHeaders(),
    body: JSON.stringify({ url, template: "quick" }),
  }),

startCheck: (url: string, kinds: string[], opts?: { dns_server?: string }) =>
  req<{ check_id: string }>("/v1/jobs/checks", {
    method: "POST",
    headers: jsonAuthHeaders(),
    body: JSON.stringify({ url, kinds, ...opts }),
  }),
};
