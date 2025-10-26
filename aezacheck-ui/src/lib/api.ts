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

export type ProfileReview = {
  id: string;
  rating: number;
  serviceId: string;
  serviceName: string;
  text: string;
  createdAt: string;
};

export type UserProfile = {
  lastCheckAt: string | null;
  reviewCount: number;
  reviews: ProfileReview[];
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

type RawProfileReview = {
  id: string;
  rating: number;
  service: { id: string; name: string } | null;
  text: string;
  created_at: string;
};

type RawUserProfile = {
  last_check_at: string | null;
  review_count: number;
  reviews: RawProfileReview[];
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

const mapProfileReview = (item: RawProfileReview): ProfileReview => ({
  id: item.id,
  rating: item.rating,
  serviceId: item.service?.id ?? "",
  serviceName: item.service?.name ?? "Неизвестный сервис",
  text: item.text ?? "",
  createdAt: item.created_at,
});

const mapProfile = (raw: RawUserProfile): UserProfile => ({
  lastCheckAt: raw.last_check_at ?? null,
  reviewCount: typeof raw.review_count === "number" ? raw.review_count : 0,
  reviews: Array.isArray(raw.reviews) ? raw.reviews.map(mapProfileReview) : [],
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

  // --- profile ---
  profile: () => call<UserProfile>("v1/profile"),

  getProfile: async () => {
    const data = await call<RawUserProfile>("v1/profile");
    return mapProfile(data);
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
};
