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

export type ServiceSummary = {
  id: string;
  name: string;
  url: string;
  status: "ok" | "warn" | "bad";
  rating: number;
  reviews: number;
  hourlyChecks: number[];
};

export type ServiceReview = {
  id: string;
  rating: number;
  text: string;
  createdAt: string;
};

export type ServiceReviewList = {
  reviews: ServiceReview[];
  reviewCount: number;
  averageRating: number;
};

export type ServiceReviewCreateResult = {
  review: ServiceReview;
  reviewCount: number;
  averageRating: number;
};

export type CheckResult = {
  id: string;
  kind: string;
  status: string;
  payload: unknown;
  metrics?: unknown;
  createdAt: string;
};

export type CheckResults = {
  checkId: string;
  status: string;
  createdAt?: string;
  startedAt?: string;
  finishedAt?: string;
  dnsServer?: string | null;
  results: CheckResult[];
};

export type MapGeo = {
  lat: number;
  lon: number;
  city?: string | null;
  country?: string | null;
  asn?: number | null;
  asn_org?: string | null;
};

export type MapAgent = {
  agentId: string;
  name: string;
  version: string;
  lastSeen: string;
  maxParallel: number;
  ip?: string | null;
  locationStr?: string | null;
  geo?: MapGeo | null;
};

export type MapTraceHop = {
  n?: number;
  ip?: string | null;
  geo?: MapGeo | null;
  [key: string]: unknown;
};

export type MapEvent = {
  type: string;
  ts?: string;
  data: Record<string, unknown>;
};

export type MapSnapshot = {
  items: MapEvent[];
  nextCursor?: string;
};

/* ================= Helpers ================= */

type RawServiceSummary = {
  id: string;
  name: string;
  url: string;
  status: string;
  rating: number;
  reviews: number;
  hourly_checks: number[];
};

type RawServiceReview = {
  id: string;
  rating: number;
  text: string;
  created_at: string;
};

type RawServiceReviewList = {
  reviews: RawServiceReview[];
  review_count: number;
  average_rating: number;
};

type RawServiceReviewCreate = {
  review: RawServiceReview;
  review_count: number;
  average_rating: number;
};

type RawCheckResult = {
  id: string;
  kind: string;
  status: string;
  payload: unknown;
  metrics?: unknown;
  created_at: string;
};

type RawCheckResults = {
  check_id: string;
  status: string;
  created_at?: string;
  started_at?: string;
  finished_at?: string;
  dns_server?: string;
  results: RawCheckResult[];
};

type RawMapGeo = {
  lat?: number | string | null;
  lon?: number | string | null;
  city?: string | null;
  country?: string | null;
  asn?: number | string | null;
  asn_org?: string | null;
};

type RawMapAgent = {
  agent_id: string;
  name: string;
  version: string;
  last_seen: string;
  max_parallel: number;
  ip?: string | null;
  location_str?: string | null;
  geo?: RawMapGeo | null;
};

type RawMapEvent = {
  type?: string;
  ts?: string;
  data?: Record<string, unknown>;
};

const API_BASE = import.meta.env.VITE_API_BASE?.replace(/\/?$/, "") ?? (import.meta.env.DEV ? "/api" : "/v1");

import { buildQuery, request } from "../services";

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

const mapServiceSummary = (item: RawServiceSummary): ServiceSummary => ({
  id: item.id,
  name: item.name,
  url: item.url,
  status: (item.status as ServiceSummary["status"]) ?? "ok",
  rating: item.rating,
  reviews: item.reviews,
  hourlyChecks: Array.isArray(item.hourly_checks) ? [...item.hourly_checks] : [],
});

const mapServiceReview = (item: RawServiceReview): ServiceReview => ({
  id: item.id,
  rating: item.rating,
  text: item.text,
  createdAt: item.created_at,
});

const mapCheckResult = (item: RawCheckResult): CheckResult => ({
  id: item.id,
  kind: item.kind,
  status: item.status,
  payload: item.payload ?? null,
  metrics: item.metrics ?? undefined,
  createdAt: item.created_at,
});

const mapCheckResults = (raw: RawCheckResults): CheckResults => ({
  checkId: raw.check_id,
  status: raw.status,
  createdAt: raw.created_at,
  startedAt: raw.started_at,
  finishedAt: raw.finished_at,
  dnsServer: raw.dns_server ?? null,
  results: Array.isArray(raw.results) ? raw.results.map(mapCheckResult) : [],
});

const mapMapGeo = (geo: RawMapGeo | null | undefined): MapGeo | null => {
  if (!geo) return null;
  const lat = Number(geo.lat);
  const lon = Number(geo.lon);
  if (!Number.isFinite(lat) || !Number.isFinite(lon)) {
    return null;
  }
  return {
    lat,
    lon,
    city: geo.city ?? null,
    country: geo.country ?? null,
    asn: geo.asn === null || geo.asn === undefined ? null : Number(geo.asn),
    asn_org: geo.asn_org ?? null,
  } satisfies MapGeo;
};

const mapMapAgent = (item: RawMapAgent): MapAgent => ({
  agentId: item.agent_id,
  name: item.name,
  version: item.version,
  lastSeen: item.last_seen,
  maxParallel: item.max_parallel,
  ip: item.ip ?? null,
  locationStr: item.location_str ?? null,
  geo: mapMapGeo(item.geo),
});

const mapMapEvent = (raw: unknown): MapEvent | null => {
  if (typeof raw === "string" && raw.trim() !== "") {
    try {
      return mapMapEvent(JSON.parse(raw));
    } catch {
      return null;
    }
  }
  if (!raw || typeof raw !== "object") {
    return null;
  }
  const evt = raw as RawMapEvent;
  const type = typeof evt.type === "string" ? evt.type : "";
  if (!type) {
    return null;
  }
  const data = evt.data && typeof evt.data === "object" ? evt.data : {};
  return { type, ts: evt.ts, data } satisfies MapEvent;
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

  // --- services ---
  listServices: async () => {
    const data = await call<{ items: RawServiceSummary[] }>("v1/services");
    return data.items.map(mapServiceSummary);
  },

  listServiceReviews: async (serviceId: string) => {
    const data = await call<RawServiceReviewList>(
      `v1/services/${encodeURIComponent(serviceId)}/reviews`
    );
    return {
      reviews: data.reviews.map(mapServiceReview),
      reviewCount: data.review_count,
      averageRating: data.average_rating,
    } satisfies ServiceReviewList;
  },

  createServiceReview: async (
    serviceId: string,
    payload: { rating: number; text: string }
  ) => {
    const data = await call<RawServiceReviewCreate>(
      `v1/services/${encodeURIComponent(serviceId)}/reviews`,
      {
        method: "POST",
        body: payload,
      }
    );
    return {
      review: mapServiceReview(data.review),
      reviewCount: data.review_count,
      averageRating: data.average_rating,
    } satisfies ServiceReviewCreateResult;
  },

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

  getCheckResults: async (checkId: string) => {
    const data = await call<RawCheckResults>(
      `v1/jobs/checks/${encodeURIComponent(checkId)}`
    );
    return mapCheckResults(data);
  },

  // --- map ---
  mapAgents: async (params: { minutes?: number; limit?: number } = {}) => {
    const search = buildQuery({ minutes: params.minutes, limit: params.limit });
    const query = search.toString();
    const suffix = query ? `?${query}` : "";
    const data = await call<{ items: RawMapAgent[] }>(`v1/map/agents${suffix}`);
    return Array.isArray(data.items) ? data.items.map(mapMapAgent) : [];
  },

  mapSnapshot: async (
    params: { minutes?: number; limit?: number; cursor?: string } = {}
  ) => {
    const search = buildQuery({
      minutes: params.minutes,
      limit: params.limit,
      cursor: params.cursor,
    });
    const query = search.toString();
    const suffix = query ? `?${query}` : "";
    const data = await call<{ items: unknown[]; next_cursor?: string }>(
      `v1/map/snapshot${suffix}`
    );
    const items = Array.isArray(data.items)
      ? data.items
          .map(mapMapEvent)
          .filter((evt): evt is MapEvent => evt !== null)
      : [];
    const nextCursor =
      typeof data.next_cursor === "string" && data.next_cursor.trim() !== ""
        ? data.next_cursor
        : undefined;
    return { items, nextCursor } satisfies MapSnapshot;
  },
};
