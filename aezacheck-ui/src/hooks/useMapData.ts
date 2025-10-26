import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  mapSSE,
  type MapAgent,
  type MapEvent,
  type MapGeo,
} from "../lib/api";

export type MapAgentLocation = {
  id: string;
  name?: string | null;
  version?: string | null;
  ip?: string | null;
  geo?: MapGeo | null;
  locationStr?: string | null;
  lastSeen?: string | null;
  lastUpdated: number;
};

export type MapEndpoint = {
  host?: string | null;
  ip?: string | null;
  geo?: MapGeo | null;
  label?: string | null;
};

export type MapHopInfo = {
  order: number;
  ip?: string | null;
  geo?: MapGeo | null;
  rttMs?: number | null;
  raw?: Record<string, unknown>;
};

export type MapCheckAgent = {
  id?: string | null;
  name?: string | null;
  ip?: string | null;
  geo?: MapGeo | null;
  version?: string | null;
};

export type MapCheckInfo = {
  id: string;
  status: "running" | "ok" | "fail";
  source?: MapEndpoint;
  target?: MapEndpoint;
  hops: MapHopInfo[];
  reason?: string | null;
  agent?: MapCheckAgent;
  updatedAt: number;
};

export type MapDataState = {
  agents: MapAgentLocation[];
  checks: MapCheckInfo[];
  isLoading: boolean;
};

type EventData = Record<string, unknown>;

type TraceInput = unknown;

type AgentInput = {
  id: string;
  name?: string | null;
  version?: string | null;
  ip?: string | null;
  geo?: MapGeo | null;
  locationStr?: string | null;
  lastSeen?: string | null;
  timestamp?: string | number | null;
};

const toTimestamp = (value: string | number | null | undefined): number | undefined => {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === "string" && value.trim() !== "") {
    const parsed = Date.parse(value);
    if (Number.isFinite(parsed)) {
      return parsed;
    }
  }
  return undefined;
};

const parseGeoPoint = (value: unknown): MapGeo | null => {
  if (!value || typeof value !== "object") return null;
  const geo = value as Record<string, unknown>;
  const lat = Number(geo.lat);
  const lon = Number(geo.lon);
  if (!Number.isFinite(lat) || !Number.isFinite(lon)) return null;
  return {
    lat,
    lon,
    city: typeof geo.city === "string" ? geo.city : null,
    country: typeof geo.country === "string" ? geo.country : null,
    country_name:
      typeof geo.country_name === "string" ? geo.country_name : null,
    asn:
      geo.asn === null || geo.asn === undefined
        ? null
        : Number.isFinite(Number(geo.asn))
        ? Number(geo.asn)
        : null,
    asn_org: typeof geo.asn_org === "string" ? geo.asn_org : null,
  } satisfies MapGeo;
};

const buildAgentLocation = (input: AgentInput): MapAgentLocation => {
  const timestamp = toTimestamp(input.timestamp ?? input.lastSeen);
  const lastUpdated = timestamp ?? Date.now();
  return {
    id: input.id,
    name: input.name ?? null,
    version: input.version ?? null,
    ip: input.ip ?? null,
    geo: input.geo ?? null,
    locationStr: input.locationStr ?? null,
    lastSeen: input.lastSeen ?? (typeof input.timestamp === "string" ? input.timestamp : null),
    lastUpdated,
  } satisfies MapAgentLocation;
};

const mergeEndpoint = (prev: MapEndpoint | undefined, next: MapEndpoint | undefined): MapEndpoint | undefined => {
  if (!prev && !next) return undefined;
  if (!prev) return next;
  if (!next) return prev;
  return {
    host: next.host ?? prev.host ?? null,
    ip: next.ip ?? prev.ip ?? null,
    geo: next.geo ?? prev.geo ?? null,
    label: next.label ?? prev.label ?? null,
  } satisfies MapEndpoint;
};

const mergeAgent = (prev: MapCheckAgent | undefined, next: MapCheckAgent | undefined): MapCheckAgent | undefined => {
  if (!prev && !next) return undefined;
  if (!prev) return next;
  if (!next) return prev;
  return {
    id: next.id ?? prev.id ?? null,
    name: next.name ?? prev.name ?? null,
    ip: next.ip ?? prev.ip ?? null,
    geo: next.geo ?? prev.geo ?? null,
    version: next.version ?? prev.version ?? null,
  } satisfies MapCheckAgent;
};

const parseEndpoint = (value: unknown): MapEndpoint | undefined => {
  if (!value || typeof value !== "object") return undefined;
  const obj = value as Record<string, unknown>;
  const host = typeof obj.host === "string" && obj.host.trim() !== "" ? obj.host : undefined;
  const ip = typeof obj.ip === "string" && obj.ip.trim() !== "" ? obj.ip : undefined;
  const label = typeof obj.location === "string" && obj.location.trim() !== "" ? obj.location : undefined;
  const geo = parseGeoPoint(obj.geo);
  if (!host && !ip && !label && !geo) return undefined;
  return {
    host: host ?? null,
    ip: ip ?? null,
    label: label ?? null,
    geo,
  } satisfies MapEndpoint;
};

const parseCheckAgent = (value: unknown): MapCheckAgent | undefined => {
  if (!value || typeof value !== "object") return undefined;
  const obj = value as Record<string, unknown>;
  const id = typeof obj.id === "string" ? obj.id : undefined;
  const name = typeof obj.name === "string" ? obj.name : undefined;
  const ip = typeof obj.ip === "string" ? obj.ip : undefined;
  const version = typeof obj.version === "string" ? obj.version : undefined;
  const geo = parseGeoPoint(obj.geo);
  if (!id && !name && !ip && !version && !geo) return undefined;
  return {
    id: id ?? null,
    name: name ?? null,
    ip: ip ?? null,
    version: version ?? null,
    geo,
  } satisfies MapCheckAgent;
};

const extractRtt = (hop: Record<string, unknown>): number | null => {
  const fields = ["rtt_ms", "rtt", "latency_ms", "latency", "delay_ms"] as const;
  for (const key of fields) {
    const value = hop[key];
    const num = Number(value);
    if (Number.isFinite(num) && num >= 0) {
      return num;
    }
  }
  return null;
};

const parseTrace = (input: TraceInput): MapHopInfo[] => {
  const rawHops: unknown[] = Array.isArray(input)
    ? input
    : input && typeof input === "object" && Array.isArray((input as Record<string, unknown>).hops)
    ? ((input as Record<string, unknown>).hops as unknown[])
    : [];

  const hops: MapHopInfo[] = [];
  rawHops.forEach((value, index) => {
    if (!value || typeof value !== "object") return;
    const hop = value as Record<string, unknown>;
    const orderCandidates = [hop.n, hop.order, hop.hop];
    let order: number | null = null;
    for (const candidate of orderCandidates) {
      const num = Number(candidate);
      if (Number.isFinite(num) && num > 0) {
        order = num;
        break;
      }
    }
    if (order === null) {
      order = index + 1;
    }
    const ipCandidate = [hop.ip, hop.addr, hop.address].find(
      (candidate) => typeof candidate === "string" && candidate.trim() !== ""
    ) as string | undefined;
    const geo = parseGeoPoint(hop.geo);
    const rttMs = extractRtt(hop);
    hops.push({
      order,
      ip: ipCandidate ?? null,
      geo,
      rttMs,
      raw: hop,
    });
  });

  return hops.sort((a, b) => a.order - b.order);
};

const normalizeStatus = (status: unknown, fallback: MapCheckInfo["status"]): MapCheckInfo["status"] => {
  if (typeof status !== "string") return fallback;
  const value = status.trim().toLowerCase();
  if (value === "ok" || value === "success" || value === "passed") return "ok";
  if (value === "running" || value === "pending" || value === "in_progress") return "running";
  if (value === "done" || value === "completed") return fallback;
  if (value === "fail" || value === "failed" || value === "error" || value === "timeout") return "fail";
  return fallback;
};

const parseMapEvent = (raw: string): MapEvent | null => {
  try {
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return null;
    const record = parsed as Record<string, unknown>;
    const type = typeof record.type === "string" ? record.type : "";
    if (!type) return null;
    const ts = typeof record.ts === "string" ? record.ts : undefined;
    const data = record.data && typeof record.data === "object" ? (record.data as Record<string, unknown>) : {};
    return { type, ts, data } satisfies MapEvent;
  } catch {
    return null;
  }
};

export function useMapData(): MapDataState {
  const [agents, setAgents] = useState<MapAgentLocation[]>([]);
  const [checks, setChecks] = useState<MapCheckInfo[]>([]);
  const [isLoading, setIsLoading] = useState(true);

  const agentsRef = useRef(new Map<string, MapAgentLocation>());
  const checksRef = useRef(new Map<string, MapCheckInfo>());

  const commitAgents = useCallback(() => {
    const next = Array.from(agentsRef.current.values()).sort(
      (a, b) => (b.lastUpdated ?? 0) - (a.lastUpdated ?? 0)
    );
    setAgents(next);
  }, []);

  const commitChecks = useCallback(() => {
    const next = Array.from(checksRef.current.values()).sort(
      (a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0)
    );
    setChecks(next);
  }, []);

  const upsertAgent = useCallback(
    (agent: MapAgentLocation) => {
      const current = agentsRef.current.get(agent.id);
      const merged: MapAgentLocation = {
        ...current,
        ...agent,
        geo: agent.geo ?? current?.geo ?? null,
        lastSeen: agent.lastSeen ?? current?.lastSeen ?? null,
        lastUpdated: agent.lastUpdated ?? current?.lastUpdated ?? Date.now(),
      } satisfies MapAgentLocation;
      agentsRef.current.set(agent.id, merged);
      commitAgents();
    },
    [commitAgents]
  );

  const upsertCheck = useCallback(
    (id: string, updater: (current: MapCheckInfo | undefined) => MapCheckInfo | undefined) => {
      const current = checksRef.current.get(id);
      const next = updater(current);
      if (!next) {
        checksRef.current.delete(id);
      } else {
        checksRef.current.set(id, next);
      }
      commitChecks();
    },
    [commitChecks]
  );

  const applyEvent = useCallback(
    (event: MapEvent) => {
      const data = (event.data as EventData) ?? {};
      const timestamp = toTimestamp(event.ts) ?? Date.now();
      switch (event.type) {
        case "agent.online": {
          const agent = data.agent as Record<string, unknown> | undefined;
          if (!agent) break;
          const id = typeof agent.id === "string" ? agent.id : undefined;
          if (!id) break;
          const location = buildAgentLocation({
            id,
            name: typeof agent.name === "string" ? agent.name : null,
            version: typeof agent.version === "string" ? agent.version : null,
            ip: typeof agent.ip === "string" ? agent.ip : null,
            geo: parseGeoPoint(agent.geo),
            locationStr: typeof agent.location === "string" ? agent.location : null,
            lastSeen: typeof data.last_seen === "string" ? data.last_seen : undefined,
            timestamp,
          });
          upsertAgent(location);
          break;
        }
        case "check.start": {
          const checkIdRaw = data.check_id ?? data.id;
          const checkId = typeof checkIdRaw === "string" ? checkIdRaw : undefined;
          if (!checkId) break;
          const source = parseEndpoint(data.source);
          const target = parseEndpoint(data.target);
          upsertCheck(checkId, (current) => {
            const base: MapCheckInfo = current
              ? { ...current }
              : { id: checkId, status: "running", hops: [], updatedAt: timestamp };
            base.status = "running";
            base.updatedAt = timestamp;
            base.reason = null;
            base.source = mergeEndpoint(base.source, source);
            base.target = mergeEndpoint(base.target, target);
            if (!current) {
              base.hops = [];
            }
            return base;
          });
          break;
        }
        case "check.result": {
          const checkIdRaw = data.check_id ?? data.id;
          const checkId = typeof checkIdRaw === "string" ? checkIdRaw : undefined;
          if (!checkId) break;
          const trace = parseTrace(data.trace);
          const target = parseEndpoint(data.target);
          const source = parseEndpoint(data.source);
          const agent = parseCheckAgent(data.agent);
          const reason = typeof data.reason === "string" && data.reason.trim() !== "" ? data.reason.trim() : null;
          const status = normalizeStatus(data.status, "running");
          upsertCheck(checkId, (current) => {
            const base: MapCheckInfo = current
              ? { ...current, hops: [...current.hops] }
              : { id: checkId, status: "running", hops: [], updatedAt: timestamp };
            base.status = status;
            base.updatedAt = timestamp;
            if (trace.length > 0) {
              base.hops = trace;
            }
            base.target = mergeEndpoint(base.target, target);
            base.source = mergeEndpoint(base.source, source);
            base.agent = mergeAgent(base.agent, agent);
            if (reason) {
              base.reason = reason;
            }
            return base;
          });
          break;
        }
        case "check.done": {
          const checkIdRaw = data.check_id ?? data.id;
          const checkId = typeof checkIdRaw === "string" ? checkIdRaw : undefined;
          if (!checkId) break;
          const source = parseEndpoint(data.source);
          upsertCheck(checkId, (current) => {
            const base: MapCheckInfo = current
              ? { ...current }
              : { id: checkId, status: "running", hops: [], updatedAt: timestamp };
            const fallback = current?.status ?? base.status;
            base.status = normalizeStatus(data.status, fallback);
            base.updatedAt = timestamp;
            base.source = mergeEndpoint(base.source, source);
            return base;
          });
          break;
        }
        default:
          break;
      }
    },
    [upsertAgent, upsertCheck]
  );

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [agentsResp, snapshotResp] = await Promise.all([
          api.mapAgents({ minutes: 60, limit: 200 }).catch<MapAgent[]>(() => []),
          api
            .mapSnapshot({ minutes: 60, limit: 500 })
            .catch(() => ({ items: [] as MapEvent[] })),
        ]);

        if (cancelled) return;

        if (agentsResp && agentsResp.length > 0) {
          const map = new Map<string, MapAgentLocation>(agentsRef.current);
          for (const agent of agentsResp) {
            map.set(
              agent.agentId,
              buildAgentLocation({
                id: agent.agentId,
                name: agent.name,
                version: agent.version,
                ip: agent.ip,
                geo: agent.geo ?? null,
                locationStr: agent.locationStr,
                lastSeen: agent.lastSeen,
              })
            );
          }
          agentsRef.current = map;
          commitAgents();
        }

        if (snapshotResp?.items) {
          for (const event of snapshotResp.items) {
            applyEvent(event);
          }
        }
      } catch (error) {
        console.error("map bootstrap failed", error);
      } finally {
        if (!cancelled) {
          setIsLoading(false);
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [applyEvent, commitAgents]);

  useEffect(() => {
    const es = mapSSE();
    const handler = (event: MessageEvent<string>) => {
      const parsed = parseMapEvent(event.data);
      if (parsed) {
        applyEvent(parsed);
      }
    };
    es.addEventListener("update", handler as EventListener);
    es.onmessage = handler;
    es.onerror = () => {
      /* ignore SSE errors */
    };
    return () => {
      es.removeEventListener("update", handler as EventListener);
      es.close();
    };
  }, [applyEvent]);

  return useMemo(
    () => ({
      agents,
      checks,
      isLoading,
    }),
    [agents, checks, isLoading]
  );
}

