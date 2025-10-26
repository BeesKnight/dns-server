// src/components/Map2D.tsx
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent } from "react";
import { geoMercator, geoPath, geoGraticule10, type GeoProjection } from "d3-geo";
import { feature } from "topojson-client";
import type {
  FeatureCollection,
  Feature,
  GeoJsonProperties,
  Geometry,
  MultiPolygon,
  Polygon,
} from "geojson";
import { useMapData, type MapAgentLocation, type MapCheckInfo } from "../hooks/useMapData";

const BORDER_STROKE = "rgba(148, 210, 255, 0.35)";
const GRID_STROKE = "rgba(56,189,248,.12)";
const LAND_FILL_A = "#124f4c";
const LAND_FILL_B = "#15524f";
const HOVER_FILL = "#1b6b66";
const USER_MARKER_GLOW = "rgba(249, 115, 22, 0.32)";
const USER_MARKER_RING = "rgba(249, 115, 22, 0.16)";
const USER_MARKER_FILL = "#fff7ed";
const USER_MARKER_STROKE = "#f97316";
const STATUS_COLORS: Record<MapCheckInfo["status"], string> = {
  running: "#9b5de5",
  ok: "#22c55e",
  fail: "#ef4444",
};
const TARGET_COLORS: Record<MapCheckInfo["status"], string> = {
  running: "#facc15",
  ok: "#22c55e",
  fail: "#f97316",
};
const AGENT_COLOR = "#59a5ff";

type WorldFeature = Feature<Polygon | MultiPolygon, { NAME?: string; name?: string }>;

type GeoLike = {
  lat?: number | null;
  lon?: number | null;
  city?: string | null;
  country?: string | null;
  label?: string | null;
};

type MapMarker = {
  id: string;
  kind: "agent" | "source" | "hop" | "target";
  lon: number;
  lat: number;
  status: MapCheckInfo["status"];
  title: string;
  subtitle?: string;
  meta?: string[];
  order?: number;
};

type MapSegment = {
  id: string;
  from: [number, number];
  to: [number, number];
  status: MapCheckInfo["status"];
};

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

type UserLocation = {
  lat: number;
  lon: number;
  label?: string;
};

type HoverState =
  | { type: "country"; name: string; x: number; y: number }
  | { type: "marker"; x: number; y: number; marker: MapMarker }
  | { type: "user"; x: number; y: number; title: string; subtitle?: string };

type Props = {
  /** на сколько пикселей опустить карту вниз */
  offsetTop?: number;
  userLocation?: UserLocation | null;
};

type GraphicsResult = {
  markers: MapMarker[];
  segments: MapSegment[];
  legend: LegendEntry[];
};

const hasCoordinates = (geo?: GeoLike | null): geo is GeoLike & { lat: number; lon: number } => {
  if (!geo) return false;
  return typeof geo.lat === "number" && Number.isFinite(geo.lat) && typeof geo.lon === "number" && Number.isFinite(geo.lon);
};

const formatGeoLocation = (geo?: GeoLike | null): string => {
  if (!geo) return "";
  const parts: string[] = [];
  if (typeof geo.city === "string" && geo.city.trim()) parts.push(geo.city.trim());
  if (typeof geo.country === "string" && geo.country.trim()) parts.push(geo.country.trim());
  return parts.join(", ");
};

const buildAgentMarker = (agent: MapAgentLocation): MapMarker | null => {
  if (!hasCoordinates(agent.geo)) return null;
  const title = agent.name?.trim() || agent.ip || `Agent ${agent.id.slice(0, 6)}`;
  const subtitle = agent.ip && agent.ip !== title ? agent.ip : undefined;
  const meta: string[] = [];
  const location = agent.locationStr?.trim() || formatGeoLocation(agent.geo);
  if (location) meta.push(location);
  return {
    id: `agent:${agent.id}`,
    kind: "agent",
    lon: agent.geo.lon,
    lat: agent.geo.lat,
    status: "running",
    title,
    subtitle,
    meta,
  } satisfies MapMarker;
};

const buildCheckGraphics = (check: MapCheckInfo): { markers: MapMarker[]; segments: MapSegment[] } => {
  const markers: MapMarker[] = [];
  const segments: MapSegment[] = [];
  const pathPoints: [number, number][] = [];

  const sourceGeo = hasCoordinates(check.source?.geo)
    ? check.source?.geo
    : hasCoordinates(check.agent?.geo)
    ? check.agent?.geo
    : undefined;

  if (sourceGeo) {
    pathPoints.push([sourceGeo.lon, sourceGeo.lat]);
    const title = check.agent?.name
      ? `Agent ${check.agent.name}`
      : check.source?.ip
      ? `Source ${check.source.ip}`
      : "Source";
    const subtitle =
      check.agent?.ip && check.agent?.ip !== check.source?.ip
        ? check.agent.ip
        : check.source?.ip && !title.includes(check.source.ip)
        ? check.source.ip
        : undefined;
    const meta: string[] = [];
    const sourceLoc = check.source?.label?.trim() || formatGeoLocation(check.source?.geo ?? check.agent?.geo);
    if (sourceLoc) meta.push(sourceLoc);
    markers.push({
      id: `source:${check.id}`,
      kind: "source",
      lon: sourceGeo.lon,
      lat: sourceGeo.lat,
      status: check.status,
      title,
      subtitle,
      meta,
    });
  }

  for (const hop of check.hops) {
    if (!hasCoordinates(hop.geo)) continue;
    pathPoints.push([hop.geo.lon, hop.geo.lat]);
    const meta: string[] = [];
    const hopLoc = formatGeoLocation(hop.geo);
    if (hopLoc) meta.push(hopLoc);
    if (typeof hop.rttMs === "number") meta.push(`RTT: ${hop.rttMs} ms`);
    if (check.status === "fail" && check.reason) meta.push(`Reason: ${check.reason}`);
    markers.push({
      id: `hop:${check.id}:${hop.order}`,
      kind: "hop",
      lon: hop.geo.lon,
      lat: hop.geo.lat,
      status: check.status,
      title: `Hop #${hop.order}`,
      subtitle: hop.ip ?? undefined,
      meta,
      order: hop.order,
    });
  }

  const targetGeo = hasCoordinates(check.target?.geo) ? check.target?.geo : undefined;
  if (targetGeo) {
    pathPoints.push([targetGeo.lon, targetGeo.lat]);
    const meta: string[] = [];
    const targetLoc = check.target?.label?.trim() || formatGeoLocation(check.target?.geo);
    if (targetLoc) meta.push(targetLoc);
    if (check.reason) meta.push(`Reason: ${check.reason}`);
    const hopWithRtt = check.hops.find((hop) => typeof hop.rttMs === "number");
    if (hopWithRtt?.rttMs !== undefined) meta.push(`Sample RTT: ${hopWithRtt.rttMs} ms`);
    markers.push({
      id: `target:${check.id}`,
      kind: "target",
      lon: targetGeo.lon,
      lat: targetGeo.lat,
      status: check.status,
      title: check.target?.host || check.target?.ip || "Target",
      subtitle: `Status: ${check.status.toUpperCase()}`,
      meta,
    });
  }

  for (let i = 0; i < pathPoints.length - 1; i += 1) {
    const from = pathPoints[i];
    const to = pathPoints[i + 1];
    segments.push({
      id: `${check.id}:${i}`,
      from,
      to,
      status: check.status,
    });
  }

  return { markers, segments };
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

const buildMapGraphics = (agents: MapAgentLocation[], checks: MapCheckInfo[]): GraphicsResult => {
  const markers: MapMarker[] = [];
  const segments: MapSegment[] = [];

  for (const agent of agents) {
    const marker = buildAgentMarker(agent);
    if (marker) markers.push(marker);
  }

  for (const check of checks) {
    const { markers: checkMarkers, segments: checkSegments } = buildCheckGraphics(check);
    markers.push(...checkMarkers);
    segments.push(...checkSegments);
  }

  return { markers, segments, legend: buildLegendEntries(checks) };
};

function useSize<T extends HTMLElement>(ref: React.RefObject<T | null>) {
  const [size, setSize] = useState({ w: 0, h: 0 });
  useEffect(() => {
    if (!ref.current) return;
    const ro = new ResizeObserver(([entry]) => {
      const rect = entry.contentRect;
      setSize({ w: Math.max(300, rect.width), h: Math.max(260, rect.height) });
    });
    ro.observe(ref.current);
    return () => ro.disconnect();
  }, [ref]);
  return size;
}

export default function Map2D({ offsetTop = 0, userLocation }: Props) {
  const wrapRef = useRef<HTMLDivElement>(null);
  const { w, h } = useSize(wrapRef);
  const { agents, checks } = useMapData();

  const [worldFc, setWorldFc] = useState<FeatureCollection | null>(null);
  const [hover, setHover] = useState<HoverState | null>(null);
  const [pan, setPan] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const [isDragging, setIsDragging] = useState(false);
  const dragState = useRef({ active: false, startX: 0, startY: 0, baseX: 0, baseY: 0 });

  useEffect(() => {
    let alive = true;
    (async () => {
      const topo = await fetch("https://cdn.jsdelivr.net/npm/world-atlas@2/countries-110m.json").then((r) => r.json());
      if (!alive) return;
      const fc = feature(
        topo,
        topo.objects.countries
      ) as unknown as FeatureCollection<Geometry, GeoJsonProperties>;
      setWorldFc(fc);
    })();
    return () => {
      alive = false;
    };
  }, []);

  const projection: GeoProjection = useMemo(() => {
    const mercator = geoMercator().center([0, 0]);
    if (w <= 0 || h <= 0) {
      return mercator.translate([0, 0]).scale(1);
    }

    const scaleForWidth = (w / (2 * Math.PI)) * 0.95;
    const scaleForHeight = (h / Math.PI) * 0.95;

    return mercator.translate([w / 2, h / 2]).scale(Math.min(scaleForWidth, scaleForHeight));
  }, [w, h]);

  const path = useMemo(() => geoPath(projection), [projection]);
  const graticule = useMemo(() => geoGraticule10(), []);

  const projectPoint = useMemo(() => {
    return (lng: number, lat: number): [number, number] => projection([lng, lat]) as [number, number];
  }, [projection]);

  const createArcPath = useMemo(() => {
    return (from: [number, number], to: [number, number]) => {
      const [sx, sy] = projectPoint(from[0], from[1]);
      const [tx, ty] = projectPoint(to[0], to[1]);
      const dx = tx - sx;
      const dy = ty - sy;
      const distance = Math.sqrt(dx * dx + dy * dy) || 1;
      const curvature = Math.min(0.45, 0.12 + distance / 2200);
      const mx = sx + dx / 2;
      const my = sy + dy / 2;
      const nx = -dy / distance;
      const ny = dx / distance;
      const cx = mx + nx * distance * curvature;
      const cy = my + ny * distance * curvature;
      return `M ${sx.toFixed(2)},${sy.toFixed(2)} C ${cx.toFixed(2)},${cy.toFixed(2)} ${cx.toFixed(2)},${cy.toFixed(2)} ${tx.toFixed(2)},${ty.toFixed(2)}`;
    };
  }, [projectPoint]);

  const graphics = useMemo(() => buildMapGraphics(agents, checks), [agents, checks]);

  const projectedMarkers = useMemo(() => {
    return graphics.markers
      .map((marker) => {
        const [x, y] = projectPoint(marker.lon, marker.lat);
        if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
        return { ...marker, x, y };
      })
      .filter((marker): marker is MapMarker & { x: number; y: number } => marker !== null);
  }, [graphics.markers, projectPoint]);

  const segmentPaths = useMemo(() => {
    return graphics.segments.map((segment) => ({
      id: segment.id,
      d: createArcPath(segment.from, segment.to),
      status: segment.status,
    }));
  }, [graphics.segments, createArcPath]);

  const userMarker = useMemo(() => {
    if (!userLocation) return null;
    const { lat, lon, label } = userLocation;
    if (!Number.isFinite(lat) || !Number.isFinite(lon)) return null;
    const [x, y] = projectPoint(lon, lat);
    if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
    const subtitle = label?.trim() ? label.trim() : undefined;
    return {
      x,
      y,
      title: "Ваше подключение",
      subtitle,
    };
  }, [projectPoint, userLocation]);

  const onEnterCountry = (e: ReactMouseEvent<SVGPathElement>, f: WorldFeature) => {
    const name = f.properties?.NAME || f.properties?.name || "";
    const svgRect = (e.currentTarget as SVGPathElement).ownerSVGElement!.getBoundingClientRect();
    setHover({ type: "country", name, x: e.clientX - svgRect.left + 12, y: e.clientY - svgRect.top + 12 });
  };
  const onLeaveCountry = () => setHover(null);

  const handleMarkerEnter = (event: ReactMouseEvent<SVGGElement>, marker: MapMarker & { x: number; y: number }) => {
    const svgRect = event.currentTarget.ownerSVGElement?.getBoundingClientRect();
    if (!svgRect) return;
    setHover({
      type: "marker",
      marker,
      x: event.clientX - svgRect.left + 12,
      y: event.clientY - svgRect.top + 12,
    });
  };

  const handleMarkerLeave = () => setHover(null);

  const onSvgMove = (e: ReactMouseEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    setHover((prev) => {
      if (!prev) return prev;
      return { ...prev, x: e.clientX - rect.left + 12, y: e.clientY - rect.top + 12 };
    });
  };
  const onSvgLeave = () => setHover(null);

  const handleDragMove = useCallback((event: MouseEvent) => {
    if (!dragState.current.active) return;
    const dx = event.clientX - dragState.current.startX;
    const dy = event.clientY - dragState.current.startY;
    setPan({ x: dragState.current.baseX + dx, y: dragState.current.baseY + dy });
  }, []);

  const handleDragUp = useCallback(() => {
    if (!dragState.current.active) return;
    dragState.current.active = false;
    setIsDragging(false);
    window.removeEventListener("mousemove", handleDragMove);
    window.removeEventListener("mouseup", handleDragUp);
  }, [handleDragMove]);

  const handleSvgMouseDown = useCallback(
    (event: ReactMouseEvent<SVGSVGElement>) => {
      if (event.button !== 0) return;
      event.preventDefault();
      dragState.current = {
        active: true,
        startX: event.clientX,
        startY: event.clientY,
        baseX: pan.x,
        baseY: pan.y,
      };
      setIsDragging(true);
      window.addEventListener("mousemove", handleDragMove);
      window.addEventListener("mouseup", handleDragUp);
    },
    [handleDragMove, handleDragUp, pan.x, pan.y]
  );

  useEffect(() => {
    return () => {
      window.removeEventListener("mousemove", handleDragMove);
      window.removeEventListener("mouseup", handleDragUp);
    };
  }, [handleDragMove, handleDragUp]);

  const handleUserEnter = (event: ReactMouseEvent<SVGCircleElement>) => {
    if (!userMarker) return;
    const svgRect = event.currentTarget.ownerSVGElement?.getBoundingClientRect();
    if (!svgRect) return;
    setHover({
      type: "user",
      title: userMarker.title,
      subtitle: userMarker.subtitle,
      x: event.clientX - svgRect.left + 14,
      y: event.clientY - svgRect.top + 14,
    });
  };

  const handleUserLeave = () => setHover(null);

  const userPulseDuration = useMemo(() => "4s", []);

  const getMarkerFill = (marker: MapMarker) => {
    if (marker.kind === "agent") return AGENT_COLOR;
    if (marker.kind === "target") return TARGET_COLORS[marker.status] ?? TARGET_COLORS.running;
    return STATUS_COLORS[marker.status] ?? STATUS_COLORS.running;
  };

  const getMarkerStroke = (marker: MapMarker) => {
    if (marker.kind === "agent") return "rgba(59,130,246,0.65)";
    if (marker.kind === "target") return "rgba(250,204,21,0.85)";
    return "rgba(148,210,255,0.75)";
  };

  const getMarkerRadius = (marker: MapMarker) => {
    switch (marker.kind) {
      case "agent":
        return 6.5;
      case "source":
        return 6;
      case "target":
        return 7.5;
      default:
        return 5;
    }
  };

  return (
    <div style={{ marginTop: offsetTop }} className="w-full">
      <div
        ref={wrapRef}
        style={{
          position: "relative",
          width: "100%",
          aspectRatio: "2 / 1",
          minHeight: 360,
          overflow: "hidden",
          borderRadius: 12,
          background:
            "radial-gradient(circle at 20% 30%, rgba(34,197,247,0.22), transparent 55%), " +
            "radial-gradient(circle at 80% 15%, rgba(56,189,248,0.18), transparent 60%), " +
            "linear-gradient(180deg,#050b1a 0%, #09142a 55%, #03070f 100%)",
        }}
      >
        <style>
          {`
            @keyframes nodePulse {
              0% { transform: scale(0.85); opacity: 0.75; }
              50% { transform: scale(1.1); opacity: 1; }
              100% { transform: scale(0.85); opacity: 0.75; }
            }
            .map-node {
              transform-origin: center;
              animation-name: nodePulse;
              animation-iteration-count: infinite;
              will-change: transform, opacity;
            }
          `}
        </style>
        <svg
          width={w}
          height={h}
          onMouseMove={onSvgMove}
          onMouseLeave={onSvgLeave}
          onMouseDown={handleSvgMouseDown}
          style={{ cursor: isDragging ? "grabbing" : "grab" }}
        >
          <defs>
            <radialGradient id="oceanGlow" cx="50%" cy="45%" r="70%">
              <stop offset="0%" stopColor="rgba(12, 74, 110, 0.85)" />
              <stop offset="60%" stopColor="rgba(3, 7, 18, 0.9)" />
              <stop offset="100%" stopColor="rgba(1, 4, 12, 1)" />
            </radialGradient>
            <radialGradient id="polarGlow" cx="70%" cy="20%" r="50%">
              <stop offset="0%" stopColor="rgba(56,189,248,0.35)" />
              <stop offset="100%" stopColor="rgba(56,189,248,0)" />
            </radialGradient>
            <filter id="landHalo" x="-20%" y="-20%" width="140%" height="140%">
              <feGaussianBlur in="SourceGraphic" stdDeviation="10" result="blur" />
              <feColorMatrix
                in="blur"
                type="matrix"
                values="0 0 0 0 0.2  0 0 0 0 0.95  0 0 0 0 0.85  0 0 0 0.55 0"
                result="halo"
              />
              <feBlend in="SourceGraphic" in2="halo" mode="screen" />
            </filter>
            <mask id="oceanMask">
              <rect width="100%" height="100%" fill="url(#oceanGlow)" />
            </mask>
          </defs>

          <rect width={w} height={h} fill="url(#oceanGlow)" />
          <rect width={w} height={h} fill="url(#polarGlow)" mask="url(#oceanMask)" opacity={0.6} />
          <g transform={`translate(${pan.x},${pan.y})`}>
            <path d={path(graticule) || ""} fill="none" stroke={GRID_STROKE} strokeWidth={0.6} opacity={0.6} />

            {worldFc &&
              (worldFc.features as WorldFeature[]).map((f, i) => (
                <path
                  key={(f.id as string) || i}
                  d={path(f) || ""}
                  fill={i % 2 === 0 ? LAND_FILL_A : LAND_FILL_B}
                  stroke={BORDER_STROKE}
                  strokeWidth={0.6}
                  filter="url(#landHalo)"
                  style={{ transition: "fill .18s ease", cursor: isDragging ? "grabbing" : "pointer" }}
                  onMouseEnter={(e) => onEnterCountry(e, f)}
                  onMouseLeave={onLeaveCountry}
                  onMouseOver={(e) => ((e.currentTarget as SVGPathElement).style.fill = HOVER_FILL)}
                  onMouseOut={(e) =>
                    ((e.currentTarget as SVGPathElement).style.fill = i % 2 === 0 ? LAND_FILL_A : LAND_FILL_B)
                  }
                />
              ))}

            {segmentPaths.map((segment) => (
              <path
                key={segment.id}
                d={segment.d}
                fill="none"
                stroke={STATUS_COLORS[segment.status] ?? STATUS_COLORS.running}
                strokeWidth={2.4}
                strokeLinecap="round"
                strokeOpacity={0.75}
              />
            ))}

            {projectedMarkers.map((marker) => (
              <g
                key={marker.id}
                transform={`translate(${marker.x},${marker.y})`}
                onMouseEnter={(event) => handleMarkerEnter(event, marker)}
                onMouseLeave={handleMarkerLeave}
                style={{ cursor: isDragging ? "grabbing" : "pointer" }}
              >
                <circle
                  r={getMarkerRadius(marker)}
                  fill={getMarkerFill(marker)}
                  stroke={getMarkerStroke(marker)}
                  strokeWidth={1.6}
                  opacity={0.92}
                />
                {marker.kind === "hop" && typeof marker.order === "number" && (
                  <text
                    textAnchor="middle"
                    dy={3}
                    fontSize={9}
                    fontWeight={600}
                    fill="#0f172a"
                  >
                    {marker.order}
                  </text>
                )}
              </g>
            ))}

            {userMarker && (
              <g transform={`translate(${userMarker.x},${userMarker.y})`}>
                <circle r={18} fill={USER_MARKER_RING} opacity={0.75} pointerEvents="none" />
                <circle
                  r={11}
                  fill={USER_MARKER_GLOW}
                  className="map-node"
                  style={{ animationDuration: userPulseDuration, animationDelay: "0s" }}
                  pointerEvents="none"
                />
                <circle
                  r={7}
                  fill={USER_MARKER_FILL}
                  stroke={USER_MARKER_STROKE}
                  strokeWidth={1.8}
                  className="map-node"
                  style={{
                    animationDuration: userPulseDuration,
                    animationDelay: "0s",
                    cursor: isDragging ? "grabbing" : "pointer",
                  }}
                  onMouseEnter={handleUserEnter}
                  onMouseLeave={handleUserLeave}
                />
                <circle r={3.2} fill={USER_MARKER_STROKE} pointerEvents="none" />
              </g>
            )}
          </g>
        </svg>

        {graphics.legend.length > 0 && (
          <div
            style={{
              position: "absolute",
              top: 16,
              left: 16,
              width: "min(340px, 32%)",
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
              {graphics.legend.map((entry, idx) => (
                <div
                  key={entry.id}
                  style={{
                    display: "grid",
                    gap: 6,
                    paddingBottom: idx === graphics.legend.length - 1 ? 0 : 8,
                    borderBottom:
                      idx === graphics.legend.length - 1
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
                  <div style={{ fontSize: 12 }}>Hops: {entry.hopCount > 0 ? entry.hopCount : "—"}</div>
                  {entry.firstHop && (
                    <div style={{ fontSize: 12, color: "#cbd5f5" }}>First hop: {entry.firstHop}</div>
                  )}
                  {typeof entry.sampleRtt === "number" && (
                    <div style={{ fontSize: 12, color: "#cbd5f5" }}>Sample RTT: {entry.sampleRtt} ms</div>
                  )}
                  {entry.reason && <div style={{ fontSize: 12, color: "#fca5a5" }}>Reason: {entry.reason}</div>}
                </div>
              ))}
            </div>
          </div>
        )}

        {hover && (
          <div
            style={{
              position: "absolute",
              left: hover.x,
              top: hover.y,
              pointerEvents: "none",
              background: "rgba(6,7,12,.9)",
              color: "#e2e8f0",
              padding: "8px 12px",
              borderRadius: 8,
              fontSize: 12,
              border: "1px solid rgba(56,189,248,.25)",
              boxShadow: "0 10px 25px rgba(2,6,23,.55)",
              maxWidth: 240,
            }}
          >
            {hover.type === "country" && <div>{hover.name}</div>}
            {hover.type === "user" && (
              <div style={{ display: "grid", gap: 4 }}>
                <div style={{ fontWeight: 600 }}>{hover.title}</div>
                {hover.subtitle && <div style={{ color: "#94a3b8" }}>{hover.subtitle}</div>}
              </div>
            )}
            {hover.type === "marker" && (
              <div style={{ display: "grid", gap: 4 }}>
                <div style={{ fontWeight: 600 }}>{hover.marker.title}</div>
                {hover.marker.subtitle && <div style={{ color: "#94a3b8" }}>{hover.marker.subtitle}</div>}
                {hover.marker.meta?.map((line, idx) => (
                  <div key={`${hover.marker.id}-meta-${idx}`}>{line}</div>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

