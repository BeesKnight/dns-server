export type GeoPoint = {
  lat: number | null;
  lon: number | null;
  city: string | null;
  country: string | null;
  asn: number | null;
  asn_org: string | null;
};

export type CheckEndpoint = {
  ip?: string;
  host?: string;
  geo?: GeoPoint | null;
};

export type CheckAgent = {
  id?: string;
  external_id?: number;
  name?: string;
  status?: string;
  ip?: string;
  geo?: GeoPoint | null;
};

export type CheckTraceHop = {
  n?: number;
  ip?: string;
  geo?: GeoPoint | null;
};

export type MapEventEnvelope<Type extends string, Payload> = {
  type: Type;
  ts?: string;
  data: Payload;
};

export type CheckStartData = {
  check_id: string;
  kind: string;
  status?: string;
  site_id?: string;
  source?: CheckEndpoint;
  target?: CheckEndpoint;
  agent?: CheckAgent;
};

export type CheckResultData = {
  check_id: string;
  kind: string;
  status?: string;
  site_id?: string;
  source?: CheckEndpoint;
  target?: CheckEndpoint;
  agent?: CheckAgent;
  trace?: CheckTraceHop[];
  reason?: string;
};

export type CheckDoneData = {
  check_id: string;
  status?: string;
  site_id?: string;
  source?: CheckEndpoint;
};

export type CheckStartEvent = MapEventEnvelope<"check.start", CheckStartData>;
export type CheckResultEvent = MapEventEnvelope<"check.result", CheckResultData>;
export type CheckDoneEvent = MapEventEnvelope<"check.done", CheckDoneData>;

export type CheckEventEnvelope = CheckStartEvent | CheckResultEvent | CheckDoneEvent;

export type CheckGeoTarget = {
  kind: string;
  ip?: string;
  host?: string;
  geo?: GeoPoint | null;
};

export type CheckGeoTrace = {
  hops: CheckTraceHop[];
};

export type CheckGeoResponse = {
  check_id: string;
  source?: CheckEndpoint;
  targets: CheckGeoTarget[];
  trace?: CheckGeoTrace;
  agent?: CheckAgent;
};
