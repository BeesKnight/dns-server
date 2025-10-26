import ky, { HTTPError, KyInstance, Options } from "ky";

export type HttpMethod = "get" | "post" | "put" | "patch" | "delete";

export interface HttpRequestOptions extends Omit<Options, "json"> {
  timeoutMs?: number;
  retryLimit?: number;
  json?: unknown;
  method?: HttpMethod;
}

export interface HttpRequest<T> {
  promise: Promise<T>;
  abort: (reason?: unknown) => void;
  controller: AbortController;
}

export class ApiError extends Error {
  status?: number;
  details?: unknown;

  constructor(message: string, options: { status?: number; details?: unknown } = {}) {
    super(message);
    this.status = options.status;
    this.details = options.details;
  }
}

const toNumber = (value: string | undefined, fallback: number) => {
  const n = Number(value);
  return Number.isFinite(n) && n > 0 ? n : fallback;
};

const BASE_URL = ((): string => {
  const explicit = import.meta.env.VITE_API_BASE?.trim();
  if (explicit) return explicit.replace(/\/?$/, "");
  return import.meta.env.DEV ? "/api" : "/v1";
})();

const DEFAULT_TIMEOUT = toNumber(import.meta.env.VITE_API_TIMEOUT, 15000);
const DEFAULT_RETRIES = toNumber(import.meta.env.VITE_API_RETRY_LIMIT, 2);

const createClient = (): KyInstance =>
  ky.create({
    prefixUrl: BASE_URL,
    hooks: {
      beforeRequest: [
        (request) => {
          if (typeof window !== "undefined") {
            const token = window.localStorage.getItem("access_token");
            if (token) {
              request.headers.set("Authorization", `Bearer ${token}`);
            }
          }
          request.headers.set("Accept", "application/json");
        },
      ],
      beforeError: [
        async (error) => {
          if (error instanceof HTTPError) {
            try {
              const data = await error.response.clone().json();
              const message = data?.message || error.message;
              return new ApiError(message, { status: error.response.status, details: data });
            } catch {
              const text = await error.response.clone().text().catch(() => "");
              return new ApiError(text || error.message, { status: error.response.status });
            }
          }
          return error;
        },
      ],
    },
    retry: {
      limit: DEFAULT_RETRIES,
      methods: ["get", "post", "put", "patch", "delete"],
      statusCodes: [408, 409, 425, 429, 500, 502, 503, 504],
      backoffLimit: 2,
    },
    timeout: DEFAULT_TIMEOUT,
  });

const http = createClient();

const normalizeMethod = (method?: HttpMethod): HttpMethod => method ?? "get";

function withErrorHandling<T>(promise: Promise<T>): Promise<T> {
  return promise.catch((error) => {
    if (error instanceof ApiError) throw error;
    if (error instanceof HTTPError) {
      throw new ApiError(error.message, { status: error.response.status });
    }
    if (error instanceof Error) {
      throw new ApiError(error.message);
    }
    throw new ApiError("Unexpected error");
  });
}

export function request<T>(input: string, options: HttpRequestOptions = {}): HttpRequest<T> {
  const controller = new AbortController();
  const { timeoutMs, retryLimit, json, signal, method, ...rest } = options;

  if (signal) {
    if (signal.aborted) {
      controller.abort(signal.reason);
    } else {
      signal.addEventListener("abort", () => controller.abort(signal.reason), { once: true });
    }
  }

  const mergedOptions: Options = {
    ...rest,
    method: normalizeMethod(method),
    signal: controller.signal,
  };

  if (typeof timeoutMs === "number") mergedOptions.timeout = timeoutMs;
  if (typeof retryLimit === "number") {
    mergedOptions.retry = {
      ...http.options.retry,
      limit: retryLimit,
    };
  }
  if (json !== undefined) mergedOptions.json = json;

  const kyInstance = http.extend({});
  const promise = withErrorHandling(kyInstance(input, mergedOptions).json<T>());

  return {
    promise,
    abort: (reason?: unknown) => controller.abort(reason),
    controller,
  };
}

export function buildQuery(params: Record<string, string | number | boolean | undefined | null>): URLSearchParams {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null) continue;
    search.append(key, String(value));
  }
  return search;
}
