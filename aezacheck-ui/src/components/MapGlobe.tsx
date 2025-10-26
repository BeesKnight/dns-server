import { useEffect, useMemo, useRef, useState } from "react";
import Globe, { GlobeMethods } from "react-globe.gl";
import type { GlobeProps } from "react-globe.gl";
import { feature } from "topojson-client";
import type { FeatureCollection, Feature, GeometryCollection, MultiPolygon, Polygon } from "geojson";
import * as THREE from "three";
import { useMapData, type MapAgentLocation, type MapCheckInfo } from "../hooks/useMapData";

/* ---------- типы точек/дуг (как у вас) ---------- */
type Arc = {
  id: string;
  startLat: number;
  startLng: number;
  endLat: number;
  endLng: number;
  color: string;
  status: "running" | "ok" | "fail";
};

type Dot = {
  id: string;
  lat: number;
  lng: number;
  color: string;
  size: number;
  label?: string;
};

type UserLocation = {
  lat: number;
  lon: number;
  label?: string;
};

type MapGlobeProps = {
  userLocation?: UserLocation | null;
};

/* ---------- типы стран ---------- */
type CountryProperties = { name?: string; ADMIN?: string; name_long?: string };

type CountryFeature = Feature<Polygon | MultiPolygon, CountryProperties>;

type CountriesTopology = {
  objects: {
    countries: GeometryCollection;
  };
};

/* ---------- цвета ---------- */
const SEA_GLOW = "rgba(56,189,248,0.28)";
const LAND_CAP = "#124f4c";
const LAND_SIDE = "#15524f";
const LAND_STROKE = "rgba(56,189,248,.12)";
const MAP_BACKGROUND =
  "radial-gradient(circle at 20% 30%, rgba(34,197,247,0.22), transparent 55%), " +
  "radial-gradient(circle at 80% 15%, rgba(56,189,248,0.18), transparent 60%), " +
  "linear-gradient(180deg,#050b1a 0%, #09142a 55%, #03070f 100%)";

const USER_POINT_ID = "user-location";

const STATUS_COLORS: Record<MapCheckInfo["status"], string> = {
  running: "#9b5de5",
  ok: "#4ade80",
  fail: "#ef4444",
};

const TARGET_COLORS: Record<MapCheckInfo["status"], string> = {
  running: "#facc15",
  ok: "#22c55e",
  fail: "#f97316",
};

const AGENT_COLOR = "#59a5ff";

type LegendEntry = {
  id: string;
  target: string;
  status: MapCheckInfo["status"];
  color: string;
  hopCount: number;
  reason?: string;
  firstHop?: string;
  sampleRtt?: number | null;
};

type GeoLike = {
  lat?: number | null;
  lon?: number | null;
  city?: string | null;
  country?: string | null;
};

const hasCoordinates = (geo?: GeoLike | null): geo is GeoLike & { lat: number; lon: number } => {
  if (!geo) return false;
  return (
    typeof geo.lat === "number" && Number.isFinite(geo.lat) && typeof geo.lon === "number" && Number.isFinite(geo.lon)
  );
};

const formatGeoLocation = (geo?: GeoLike | null): string => {
  if (!geo) return "";
  const parts: string[] = [];
  if (typeof geo.city === "string" && geo.city.trim() !== "") parts.push(geo.city.trim());
  if (typeof geo.country === "string" && geo.country.trim() !== "") parts.push(geo.country.trim());
  return parts.join(", ");
};

const buildAgentPoint = (agent: MapAgentLocation): Dot | null => {
  if (!hasCoordinates(agent.geo)) return null;
  const labelParts: string[] = [];
  const displayName = agent.name?.trim() || agent.ip || agent.id;
  labelParts.push(`Agent ${displayName}`);
  if (agent.ip && agent.ip !== displayName) {
    labelParts.push(agent.ip);
  }
  const location = agent.locationStr?.trim() || formatGeoLocation(agent.geo);
  if (location) labelParts.push(location);
  return {
    id: `agent:${agent.id}`,
    lat: agent.geo.lat,
    lng: agent.geo.lon,
    color: AGENT_COLOR,
    size: 0.38,
    label: labelParts.join("\n"),
  } satisfies Dot;
};

const buildCheckGraphics = (check: MapCheckInfo): { arcs: Arc[]; points: Dot[] } => {
  const arcs: Arc[] = [];
  const points: Dot[] = [];
  const statusColor = STATUS_COLORS[check.status] ?? STATUS_COLORS.running;
  const targetColor = TARGET_COLORS[check.status] ?? TARGET_COLORS.running;

  const path: { lat: number; lon: number }[] = [];

  const sourceGeo = hasCoordinates(check.source?.geo)
    ? check.source?.geo
    : hasCoordinates(check.agent?.geo)
    ? check.agent?.geo
    : undefined;

  if (sourceGeo && hasCoordinates(sourceGeo)) {
    path.push({ lat: sourceGeo.lat, lon: sourceGeo.lon });
    const sourceParts: string[] = [];
    if (check.agent?.name) {
      sourceParts.push(`Agent ${check.agent.name}`);
    } else if (check.source?.ip) {
      sourceParts.push(`Source ${check.source.ip}`);
    }
    if (check.agent?.ip && (!check.agent.name || check.agent.ip !== check.source?.ip)) {
      sourceParts.push(check.agent.ip);
    }
    const sourceLoc = check.source?.label?.trim() || formatGeoLocation(check.source?.geo ?? check.agent?.geo);
    if (sourceLoc) sourceParts.push(sourceLoc);
    if (sourceParts.length > 0) {
      points.push({
        id: `source:${check.id}`,
        lat: sourceGeo.lat,
        lng: sourceGeo.lon,
        color: statusColor,
        size: 0.35,
        label: sourceParts.join(" • "),
      });
    }
  }

  check.hops.forEach((hop) => {
    if (!hasCoordinates(hop.geo)) return;
    path.push({ lat: hop.geo.lat, lon: hop.geo.lon });
    const labelParts: string[] = [`Hop #${hop.order}`];
    if (hop.ip) labelParts.push(hop.ip);
    const location = formatGeoLocation(hop.geo);
    if (location) labelParts.push(location);
    if (typeof hop.rttMs === "number") labelParts.push(`RTT: ${hop.rttMs} ms`);
    if (check.status === "fail" && check.reason) labelParts.push(`Reason: ${check.reason}`);
    points.push({
      id: `hop:${check.id}:${hop.order}`,
      lat: hop.geo.lat,
      lng: hop.geo.lon,
      color: statusColor,
      size: 0.28,
      label: labelParts.join(" • "),
    });
  });

  const targetGeo = hasCoordinates(check.target?.geo) ? check.target?.geo : undefined;
  if (targetGeo) {
    path.push({ lat: targetGeo.lat, lon: targetGeo.lon });
    const labelLines: string[] = [];
    const targetName = check.target?.host || check.target?.ip || "Target";
    labelLines.push(targetName);
    labelLines.push(`Status: ${check.status.toUpperCase()}`);
    const targetLoc = check.target?.label?.trim() || formatGeoLocation(check.target?.geo);
    if (targetLoc) labelLines.push(targetLoc);
    if (check.reason) labelLines.push(`Reason: ${check.reason}`);
    const hopWithRtt = check.hops.find((hop) => typeof hop.rttMs === "number");
    if (hopWithRtt?.rttMs !== undefined) {
      labelLines.push(`Sample RTT: ${hopWithRtt.rttMs} ms`);
    }
    points.push({
      id: `target:${check.id}`,
      lat: targetGeo.lat,
      lng: targetGeo.lon,
      color: targetColor,
      size: 0.46,
      label: labelLines.join("\n"),
    });
  }

  for (let i = 0; i < path.length - 1; i += 1) {
    const from = path[i];
    const to = path[i + 1];
    arcs.push({
      id: `${check.id}:${i}`,
      startLat: from.lat,
      startLng: from.lon,
      endLat: to.lat,
      endLng: to.lon,
      color: statusColor,
      status: check.status,
    });
  }

  return { arcs, points };
};

const buildLegendEntries = (checks: MapCheckInfo[]): LegendEntry[] => {
  return checks.slice(0, 6).map((check) => {
    const firstHop = check.hops[0];
    const firstHopParts: string[] = [];
    if (firstHop) {
      firstHopParts.push(`#${firstHop.order}`);
      if (firstHop.ip) firstHopParts.push(firstHop.ip);
      const loc = formatGeoLocation(firstHop.geo);
      if (loc) firstHopParts.push(loc);
    }
    const hopWithRtt = check.hops.find((hop) => typeof hop.rttMs === "number");
    return {
      id: check.id,
      target: check.target?.host || check.target?.ip || "Unknown target",
      status: check.status,
      color: STATUS_COLORS[check.status] ?? STATUS_COLORS.running,
      hopCount: check.hops.length,
      reason: check.reason ?? undefined,
      firstHop: firstHopParts.length > 0 ? firstHopParts.join(" • ") : undefined,
      sampleRtt: hopWithRtt?.rttMs ?? null,
    } satisfies LegendEntry;
  });
};

export default function MapGlobe({ userLocation }: MapGlobeProps) {
  const globeRef = useRef<GlobeMethods | undefined>(undefined);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [containerSize, setContainerSize] = useState<{ width: number; height: number }>({
    width: 0,
    height: 0,
  });

  const [polygons, setPolygons] = useState<CountryFeature[]>([]);
  const { agents, checks } = useMapData();
  const derived = useMemo(() => {
    const nextArcs: Arc[] = [];
    const nextPoints: Dot[] = [];

    for (const agent of agents) {
      const point = buildAgentPoint(agent);
      if (point) nextPoints.push(point);
    }

    for (const check of checks) {
      const { arcs: checkArcs, points: checkPoints } = buildCheckGraphics(check);
      nextArcs.push(...checkArcs);
      nextPoints.push(...checkPoints);
    }

    return { arcs: nextArcs, points: nextPoints };
  }, [agents, checks]);

  const arcs = derived.arcs;
  const points = derived.points;
  const legendEntries = useMemo(() => buildLegendEntries(checks), [checks]);

  const globeMaterial = useMemo(() => {
    const material = new THREE.MeshPhongMaterial({
      color: "#0a1e2a",
      emissive: new THREE.Color("#06121b"),
      emissiveIntensity: 0.32,
      shininess: 0,
      specular: new THREE.Color(0x000000),
    });

    material.needsUpdate = true;
    return material;
  }, []);

  /* ---------- загрузка карт ---------- */
  useEffect(() => {
    // стартовая точка обзора
    globeRef.current?.pointOfView({ lat: 30, lng: 20, altitude: 1.6 }, 1200);

    fetch("https://unpkg.com/world-atlas@2.0.2/countries-110m.json")
      .then((r) => r.json())
      .then((topologyRaw: unknown) => {
        const topo = topologyRaw as CountriesTopology;
        const fc = feature(
          topo as unknown as Parameters<typeof feature>[0],
          topo.objects.countries as unknown as Parameters<typeof feature>[1]
        ) as FeatureCollection<Polygon | MultiPolygon, CountryProperties>;

        const feats = fc.features as CountryFeature[];
        setPolygons(feats);

      })
      .catch(() => {
        // оффлайн — просто без подписей
      });
  }, []);

  useEffect(() => {
    const node = containerRef.current;
    if (!node) {
      return;
    }

    const updateSize = () => {
      setContainerSize({ width: node.clientWidth, height: node.clientHeight });
    };

    updateSize();

    if (typeof ResizeObserver !== "undefined") {
      const observer = new ResizeObserver(() => updateSize());
      observer.observe(node);
      return () => observer.disconnect();
    }

    const listener = () => updateSize();
    if (typeof window !== "undefined") {
      window.addEventListener("resize", listener);
    }
    return () => {
      if (typeof window !== "undefined") {
        window.removeEventListener("resize", listener);
      }
    };
  }, []);

  const pointLatAccessor: GlobeProps["pointLat"] = (d) => (d as Dot).lat;
  const pointLngAccessor: GlobeProps["pointLng"] = (d) => (d as Dot).lng;
  const pointColorAccessor: GlobeProps["pointColor"] = (d) => (d as Dot).color;
  const pointRadiusAccessor: GlobeProps["pointRadius"] = (d) => (d as Dot).size;
  const pointLabelAccessor: GlobeProps["pointLabel"] = (d) => (d as Dot).label || "";
  const arcStartLatAccessor: GlobeProps["arcStartLat"] = (d) => (d as Arc).startLat;
  const arcStartLngAccessor: GlobeProps["arcStartLng"] = (d) => (d as Arc).startLng;
  const arcEndLatAccessor: GlobeProps["arcEndLat"] = (d) => (d as Arc).endLat;
  const arcEndLngAccessor: GlobeProps["arcEndLng"] = (d) => (d as Arc).endLng;
  const arcColorAccessor: GlobeProps["arcColor"] = (d: object) => (d as Arc).color;

  const combinedPoints = useMemo(() => {
    const basePoints = points.filter((point) => point.id !== USER_POINT_ID);
    if (!userLocation) return basePoints;

    const { lat, lon, label } = userLocation;
    if (!Number.isFinite(lat) || !Number.isFinite(lon)) {
      return basePoints;
    }

    return [
      ...basePoints,
      {
        id: USER_POINT_ID,
        lat,
        lng: lon,
        color: "#f97316",
        size: 0.7,
        label: label ?? "Ваше подключение",
      },
    ];
  }, [points, userLocation]);

  return (
    <div className="w-full">
      <div
        style={{
          position: "relative",
          width: "100%",
          aspectRatio: "2 / 1",
          minHeight: 360,
          overflow: "hidden",
          borderRadius: 12,
      background: MAP_BACKGROUND,
    }}
  >
        {legendEntries.length > 0 && (
          <div
            style={{
              position: "absolute",
              top: 16,
              left: 16,
              width: "min(320px, 30%)",
              padding: "12px 16px",
              display: "grid",
              gap: 12,
              borderRadius: 12,
              border: "1px solid rgba(56,189,248,0.25)",
              background: "rgba(2, 6, 23, 0.74)",
              backdropFilter: "blur(12px)",
              boxShadow: "0 18px 40px rgba(2, 6, 23, 0.55)",
              color: "#e2e8f0",
              zIndex: 2,
              pointerEvents: "auto",
            }}
          >
            <div
              style={{
                fontSize: 12,
                letterSpacing: 0.8,
                textTransform: "uppercase",
                fontWeight: 600,
                color: "#bae6fd",
              }}
            >
              Active checks
            </div>
            <div style={{ display: "grid", gap: 10 }}>
              {legendEntries.map((entry, idx) => (
                <div
                  key={entry.id}
                  style={{
                    display: "grid",
                    gap: 6,
                    paddingBottom: idx === legendEntries.length - 1 ? 0 : 8,
                    borderBottom:
                      idx === legendEntries.length - 1
                        ? "none"
                        : "1px solid rgba(56,189,248,0.18)",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                    <span
                      style={{
                        width: 10,
                        height: 10,
                        borderRadius: "9999px",
                        background: entry.color,
                        boxShadow: "0 0 12px rgba(56,189,248,0.3)",
                      }}
                    />
                    <div style={{ fontWeight: 600, fontSize: 14 }}>{entry.target}</div>
                  </div>
                  <div style={{ fontSize: 12, color: "#94a3b8" }}>
                    Status: {entry.status === "ok" ? "Success" : entry.status === "fail" ? "Failed" : "Running"}
                  </div>
                  <div style={{ fontSize: 12 }}>
                    Hops: {entry.hopCount > 0 ? entry.hopCount : "—"}
                  </div>
                  {entry.firstHop && (
                    <div style={{ fontSize: 12, color: "#cbd5f5" }}>First hop: {entry.firstHop}</div>
                  )}
                  {typeof entry.sampleRtt === "number" && (
                    <div style={{ fontSize: 12, color: "#cbd5f5" }}>Sample RTT: {entry.sampleRtt} ms</div>
                  )}
                  {entry.reason && (
                    <div style={{ fontSize: 12, color: "#fca5a5" }}>Reason: {entry.reason}</div>
                  )}
                </div>
              ))}
            </div>
          </div>
        )}
        <div ref={containerRef} style={{ position: "absolute", inset: 0 }}>
          <Globe
            ref={globeRef}
            backgroundColor={"rgba(0, 0, 0, 0)"}
            globeMaterial={globeMaterial}
            width={containerSize.width || undefined}
            height={containerSize.height || undefined}

            /* атмосфера и фон */
            showAtmosphere
            atmosphereColor={SEA_GLOW}
            atmosphereAltitude={0.25}

            /* материки */
            polygonsData={polygons}
            polygonAltitude={0.02}
            polygonCapColor={() => LAND_CAP}
            polygonSideColor={() => LAND_SIDE}
            polygonStrokeColor={() => LAND_STROKE}
            polygonsTransitionDuration={0}

            polygonLabel={(feat) => {
              const props = (feat as CountryFeature).properties || {};
              return (
                (props.name as string) ||
                (props.ADMIN as string) ||
                (props.name_long as string) ||
                ""
              );
            }}

            /* точки (агенты/цели) */
            pointsData={combinedPoints}
            pointLat={pointLatAccessor}
            pointLng={pointLngAccessor}
            pointAltitude={0.01}
            pointColor={pointColorAccessor}
            pointRadius={pointRadiusAccessor}
            pointLabel={pointLabelAccessor}

            /* дуги */
            arcsData={arcs}
            arcStartLat={arcStartLatAccessor}
            arcStartLng={arcStartLngAccessor}
            arcEndLat={arcEndLatAccessor}
            arcEndLng={arcEndLngAccessor}
            arcColor={arcColorAccessor}
            arcAltitude={0.25}
            arcStroke={0.6}
            arcDashLength={0.45}
            arcDashGap={0.2}
            arcDashAnimateTime={1600}

            /* интерактив */
            enablePointerInteraction={true}
          />
        </div>
      </div>
    </div>
  );
}
