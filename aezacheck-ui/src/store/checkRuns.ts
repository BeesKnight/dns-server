import { useEffect, useSyncExternalStore } from "react";

import { api, mapSSE } from "../lib/api";
import type {
  CheckAgent,
  CheckDoneEvent,
  CheckEndpoint,
  CheckEventEnvelope,
  CheckGeoResponse,
  CheckGeoTarget,
  CheckResultEvent,
  CheckStartEvent,
  CheckTraceHop,
} from "../types/checks";

export type CheckRunStatus = "idle" | "running" | "done";

export type CheckKindState = {
  kind: string;
  status?: string;
  target?: CheckEndpoint;
  agent?: CheckAgent;
  reason?: string;
  trace?: CheckTraceHop[];
  lastEvent?: "check.start" | "check.result";
  updatedAt?: string;
};

export type CheckRun = {
  checkId: string;
  url: string;
  status: CheckRunStatus;
  startedAt: string;
  doneStatus?: string;
  source?: CheckEndpoint;
  agent?: CheckAgent;
  trace?: CheckTraceHop[];
  kinds: Record<string, CheckKindState>;
  events: number;
  geoLoading: boolean;
  geoError: string | null;
};

export type CheckRunStoreState = {
  current: CheckRun | null;
  starting: boolean;
  startError: string | null;
  streamError: string | null;
};

type Listener = () => void;

let state: CheckRunStoreState = {
  current: null,
  starting: false,
  startError: null,
  streamError: null,
};

const listeners = new Set<Listener>();
let stream: EventSource | null = null;
let streamEnabled = false;
let activeCheckId: string | null = null;
let geoRequest: { checkId: string; promise: Promise<void> } | null = null;

const notify = () => {
  for (const listener of listeners) {
    listener();
  }
};

const setState = (patch: Partial<CheckRunStoreState>) => {
  state = { ...state, ...patch };
  notify();
};

const setCurrent = (updater: (current: CheckRun | null) => CheckRun | null, extra?: Partial<CheckRunStoreState>) => {
  const next = updater(state.current);
  if (next === state.current && !extra) return;
  state = { ...state, current: next, ...(extra ?? {}) };
  notify();
};

const subscribe = (listener: Listener) => {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
};

const getSnapshot = () => state;

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null;

const mergeEndpoint = (prev?: CheckEndpoint, next?: CheckEndpoint): CheckEndpoint | undefined => {
  if (!prev && !next) return undefined;
  return {
    ...(prev ?? {}),
    ...(next ?? {}),
    geo: next?.geo ?? prev?.geo,
  };
};

const mergeAgent = (prev?: CheckAgent, next?: CheckAgent): CheckAgent | undefined => {
  if (!prev && !next) return undefined;
  return {
    ...(prev ?? {}),
    ...(next ?? {}),
    geo: next?.geo ?? prev?.geo,
  };
};

const normalizeTrace = (trace?: CheckTraceHop[] | null): CheckTraceHop[] | undefined => {
  if (!Array.isArray(trace)) return undefined;
  return trace.map((hop) => ({ ...hop }));
};

const ensureStreamClosed = () => {
  if (stream) {
    stream.close();
    stream = null;
  }
};

const ensureStream = () => {
  if (!streamEnabled || stream || !activeCheckId) {
    return;
  }
  stream = mapSSE();
  stream.onmessage = (event) => {
    try {
      const parsed = JSON.parse(event.data) as CheckEventEnvelope | Record<string, unknown>;
      if (!isRecord(parsed) || typeof parsed.type !== "string" || !parsed.data) {
        return;
      }
      const envelope = parsed as CheckEventEnvelope;
      if (envelope.type === "check.start") {
        handleCheckStart(envelope);
      } else if (envelope.type === "check.result") {
        handleCheckResult(envelope);
      } else if (envelope.type === "check.done") {
        handleCheckDone(envelope);
      }
    } catch (error) {
      void error;
    }
  };
  stream.onerror = () => {
    setState({ streamError: "Поток событий недоступен" });
  };
};

const setActiveCheck = (checkId: string | null) => {
  activeCheckId = checkId;
  geoRequest = null;
  ensureStreamClosed();
  if (checkId && streamEnabled) {
    ensureStream();
  }
};

const resetGeoState = () => {
  geoRequest = null;
};

const handleCheckStart = (event: CheckStartEvent) => {
  if (!event?.data || event.data.check_id !== activeCheckId) return;
  const { kind } = event.data;
  setCurrent((current) => {
    if (!current || current.checkId !== event.data.check_id) return current;
    const previousKind = current.kinds[kind];
    const updatedKind: CheckKindState = {
      ...(previousKind ?? {}),
      kind,
      status: event.data.status ?? previousKind?.status,
      target: mergeEndpoint(previousKind?.target, event.data.target),
      agent: mergeAgent(previousKind?.agent, event.data.agent),
      reason: undefined,
      lastEvent: "check.start",
      updatedAt: event.ts,
    };
    const updated: CheckRun = {
      ...current,
      status: "running",
      source: mergeEndpoint(current.source, event.data.source),
      kinds: { ...current.kinds, [kind]: updatedKind },
      events: current.events + 1,
    };
    return updated;
  }, { streamError: null });
};

const handleCheckResult = (event: CheckResultEvent) => {
  if (!event?.data || event.data.check_id !== activeCheckId) return;
  const { kind } = event.data;
  setCurrent((current) => {
    if (!current || current.checkId !== event.data.check_id) return current;
    const previousKind = current.kinds[kind];
    const updatedTrace = normalizeTrace(event.data.trace) ?? previousKind?.trace;
    const updatedKind: CheckKindState = {
      ...(previousKind ?? {}),
      kind,
      status: event.data.status ?? previousKind?.status,
      target: mergeEndpoint(previousKind?.target, event.data.target),
      agent: mergeAgent(previousKind?.agent, event.data.agent),
      trace: updatedTrace,
      reason: event.data.reason ?? previousKind?.reason,
      lastEvent: "check.result",
      updatedAt: event.ts,
    };
    const updated: CheckRun = {
      ...current,
      source: mergeEndpoint(current.source, event.data.source),
      trace: normalizeTrace(event.data.trace) ?? current.trace,
      kinds: { ...current.kinds, [kind]: updatedKind },
      events: current.events + 1,
    };
    return updated;
  }, { streamError: null });
};

const fetchGeo = (checkId: string) => {
  if (geoRequest && geoRequest.checkId === checkId) {
    return geoRequest.promise;
  }
  const promise = (async () => {
    setCurrent((current) => {
      if (!current || current.checkId !== checkId) return current;
      return { ...current, geoLoading: true, geoError: null };
    });
    try {
      const geo = await api.getCheckGeo(checkId);
      setCurrent((current) => applyGeoPayload(current, geo));
    } catch (error) {
      const message = error instanceof Error ? error.message : "Не удалось загрузить геоданные";
      setCurrent((current) => {
        if (!current || current.checkId !== checkId) return current;
        return { ...current, geoLoading: false, geoError: message };
      });
    }
  })().finally(() => {
    if (geoRequest?.checkId === checkId) {
      geoRequest = null;
    }
  });
  geoRequest = { checkId, promise };
  return promise;
};

const applyGeoPayload = (current: CheckRun | null, payload: CheckGeoResponse): CheckRun | null => {
  if (!current || current.checkId !== payload.check_id) return current;
  const nextKinds = { ...current.kinds };
  payload.targets?.forEach((target) => {
    nextKinds[target.kind] = mergeKindWithGeo(nextKinds[target.kind], target);
  });
  const trace = payload.trace?.hops ? payload.trace.hops.map((hop) => ({ ...hop })) : current.trace;
  return {
    ...current,
    source: mergeEndpoint(current.source, payload.source),
    agent: mergeAgent(current.agent, payload.agent),
    kinds: nextKinds,
    trace,
    geoLoading: false,
    geoError: null,
  };
};

const mergeKindWithGeo = (kindState: CheckKindState | undefined, target: CheckGeoTarget): CheckKindState => {
  const base: CheckKindState = kindState ?? { kind: target.kind };
  const endpoint: CheckEndpoint = {
    ip: target.ip,
    host: target.host,
    geo: target.geo,
  };
  return {
    ...base,
    target: mergeEndpoint(base.target, endpoint),
  };
};

const handleCheckDone = (event: CheckDoneEvent) => {
  if (!event?.data || event.data.check_id !== activeCheckId) return;
  setCurrent((current) => {
    if (!current || current.checkId !== event.data.check_id) return current;
    const updated: CheckRun = {
      ...current,
      status: "done",
      doneStatus: event.data.status ?? current.doneStatus ?? "done",
      source: mergeEndpoint(current.source, event.data.source),
      events: current.events + 1,
    };
    return updated;
  }, { streamError: null });
  fetchGeo(event.data.check_id).catch(() => {
    // Ошибка уже обработана в fetchGeo
  });
};

const startQuickCheck = async (url: string) => {
  const trimmed = url.trim();
  if (!trimmed) {
    setState({ startError: "Укажи домен или URL" });
    return { ok: false as const };
  }
  setState({ starting: true, startError: null });
  resetGeoState();
  try {
    const { check_id } = await api.startQuickCheck(trimmed);
    const run: CheckRun = {
      checkId: check_id,
      url: trimmed,
      status: "running",
      startedAt: new Date().toISOString(),
      kinds: {},
      events: 0,
      geoLoading: false,
      geoError: null,
    };
    state = {
      ...state,
      current: run,
      starting: false,
      startError: null,
      streamError: null,
    };
    notify();
    setActiveCheck(check_id);
    ensureStream();
    return { ok: true as const, checkId: check_id };
  } catch (error) {
    const message = error instanceof Error ? error.message : "Не удалось запустить проверку";
    setState({ startError: message, starting: false });
    return { ok: false as const };
  }
};

const resetState = () => {
  setActiveCheck(null);
  state = {
    current: null,
    starting: false,
    startError: null,
    streamError: null,
  };
  notify();
};

const setStreamEnabled = (enabled: boolean) => {
  if (streamEnabled === enabled) return;
  streamEnabled = enabled;
  if (!enabled) {
    ensureStreamClosed();
    return;
  }
  ensureStream();
};

export function useCheckRun(enabled: boolean) {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

  useEffect(() => {
    setStreamEnabled(enabled);
    return () => {
      if (enabled) {
        setStreamEnabled(false);
      }
    };
  }, [enabled]);

  return {
    ...snapshot,
    start: startQuickCheck,
    reset: resetState,
  };
}
