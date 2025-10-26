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

const normalizeBase = (value: string) => value.replace(/\/?$/, "");

export const resolveApiBase = (): string => {
  const explicit = import.meta.env.VITE_API_BASE?.trim();
  if (explicit) return normalizeBase(explicit);
  return normalizeBase(import.meta.env.DEV ? "/api" : "/v1");
};

const BASE_URL = resolveApiBase();

const DEFAULT_TIMEOUT = toNumber(import.meta.env.VITE_API_TIMEOUT, 15000);
const DEFAULT_RETRIES = toNumber(import.meta.env.VITE_API_RETRY_LIMIT, 2);

type RetryOptions = Exclude<Options["retry"], number | undefined>;

const DEFAULT_RETRY_CONFIG: RetryOptions = {
  limit: DEFAULT_RETRIES,
  methods: ["get", "post", "put", "patch", "delete"],
  statusCodes: [408, 409, 425, 429, 500, 502, 503, 504],
  backoffLimit: 2,
};

const createClient = (): KyInstance =>
  ky.create({
    prefixUrl: BASE_URL,
    hooks: {
      beforeRequest: [
        (request: Request) => {
          if (typeof window !== "undefined") {
            const token = window.localStorage.getItem("access_token");
            if (token) {
              request.headers.set("Authorization", `Bearer ${token}`);
            }
          }
          request.headers.set("Accept", "application/json");
        },
      ],
    },
    retry: DEFAULT_RETRY_CONFIG,
    timeout: DEFAULT_TIMEOUT,
  });

const http = createClient();

const normalizeMethod = (method?: HttpMethod): HttpMethod => method ?? "get";

async function withErrorHandling<T>(promise: Promise<T>): Promise<T> {
  try {
    return await promise;
  } catch (error: unknown) {
    if (error instanceof ApiError) throw error;
    if (error instanceof HTTPError) {
      const httpError = error as HTTPError;
      const response = httpError.response;

      try {
        const data = await response.clone().json();
        const message = typeof data?.message === "string" ? data.message : httpError.message;
        throw new ApiError(message, { status: response.status, details: data });
      } catch {
        const text = await response.clone().text().catch(() => "");
        throw new ApiError(text || httpError.message, { status: response.status });
      }
    }
    if (error instanceof Error) {
      throw new ApiError(error.message);
    }
    throw new ApiError("Unexpected error");
  }
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
    mergedOptions.retry = { ...DEFAULT_RETRY_CONFIG, limit: retryLimit } satisfies RetryOptions;
  } else {
    mergedOptions.retry = DEFAULT_RETRY_CONFIG;
  }
  if (json !== undefined) mergedOptions.json = json;

  const promise = withErrorHandling<T>(http(input, mergedOptions).json<T>());

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
