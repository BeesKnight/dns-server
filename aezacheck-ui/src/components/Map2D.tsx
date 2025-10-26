// src/components/Map2D.tsx
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent } from "react";
import {
  geoMercator,
  geoPath,
  geoGraticule10,
  geoCentroid,
  geoInterpolate,
  type GeoProjection
} from "d3-geo";
import { feature } from "topojson-client";
import type {
  FeatureCollection,
  Feature,
  GeoJsonProperties,
  Geometry,
  MultiPolygon,
  Polygon,
} from "geojson";
import { mapSSE } from "../lib/api";

const BORDER_STROKE = "rgba(148, 210, 255, 0.35)";
const GRID_STROKE = "rgba(56,189,248,.12)";
const LAND_FILL_A = "#124f4c";
const LAND_FILL_B = "#15524f";
const HOVER_FILL = "#1b6b66";
const ATTACK_COLOR = "#ff4bd6";
const ATTACK_WIDTH = 2.8;
const ATTACK_LIFETIME_MS = 2000;
const USER_MARKER_GLOW = "rgba(249, 115, 22, 0.32)";
const USER_MARKER_RING = "rgba(249, 115, 22, 0.16)";
const USER_MARKER_FILL = "#fff7ed";
const USER_MARKER_STROKE = "#f97316";

type WorldFeature = Feature<Polygon | MultiPolygon, { NAME?: string; name?: string }>;
type Attack = { id: number; from: [number, number]; to: [number, number]; };

type NetworkNode = {
  id: string;
  name: string;
  lat: number;
  lng: number;
  country: string;
  status: "operational" | "degraded" | "maintenance";
  service: string;
  responseMs: number;
  importance: number;
};

type NetworkRoute = {
  id: string;
  from: string;
  to: string;
  label: string;
  gradient: [string, string];
};

type UserLocation = {
  lat: number;
  lon: number;
  label?: string;
};

type HoverState =
  | { type: "country"; name: string; x: number; y: number }
  | {
      type: "node";
      x: number;
      y: number;
      node: NetworkNode;
    }
  | {
      type: "route";
      x: number;
      y: number;
      route: NetworkRoute;
      stats: { jitter: number; packetLoss: number };
    }
  | {
      type: "user";
      x: number;
      y: number;
      title: string;
      subtitle?: string;
    };

type Props = {
  /** на сколько пикселей опустить карту вниз */
  offsetTop?: number;
  userLocation?: UserLocation | null;
};

const BASE_NODES: NetworkNode[] = [
  {
    id: "ams-edge",
    name: "Amsterdam Edge",
    lat: 52.377,
    lng: 4.897,
    country: "Netherlands",
    status: "operational",
    service: "Recursive DNS",
    responseMs: 28,
    importance: 0.9
  },
  {
    id: "sfo-core",
    name: "San Francisco Core",
    lat: 37.7749,
    lng: -122.4194,
    country: "United States",
    status: "operational",
    service: "Authoritative DNS",
    responseMs: 34,
    importance: 1
  },
  {
    id: "gru-cache",
    name: "São Paulo Cache",
    lat: -23.5505,
    lng: -46.6333,
    country: "Brazil",
    status: "degraded",
    service: "RBL Service",
    responseMs: 61,
    importance: 0.7
  },
  {
    id: "sin-edge",
    name: "Singapore Edge",
    lat: 1.3521,
    lng: 103.8198,
    country: "Singapore",
    status: "operational",
    service: "Zone Transfer",
    responseMs: 42,
    importance: 0.85
  },
  {
    id: "dub-core",
    name: "Dublin Control",
    lat: 53.3498,
    lng: -6.2603,
    country: "Ireland",
    status: "maintenance",
    service: "Telemetry",
    responseMs: 75,
    importance: 0.6
  },
  {
    id: "bom-edge",
    name: "Mumbai Edge",
    lat: 19.076,
    lng: 72.8777,
    country: "India",
    status: "operational",
    service: "Resolver",
    responseMs: 55,
    importance: 0.65
  },
  {
    id: "jnb-observer",
    name: "Johannesburg Observer",
    lat: -26.2041,
    lng: 28.0473,
    country: "South Africa",
    status: "operational",
    service: "Latency Probe",
    responseMs: 48,
    importance: 0.5
  },
  {
    id: "syd-core",
    name: "Sydney Core",
    lat: -33.8688,
    lng: 151.2093,
    country: "Australia",
    status: "degraded",
    service: "Threat Intel",
    responseMs: 66,
    importance: 0.8
  },
  {
    id: "hnd-cache",
    name: "Tokyo Cache",
    lat: 35.6762,
    lng: 139.6503,
    country: "Japan",
    status: "operational",
    service: "DNSSEC",
    responseMs: 37,
    importance: 0.9
  },
  {
    id: "cpt-monitor",
    name: "Cape Town Monitor",
    lat: -33.9249,
    lng: 18.4241,
    country: "South Africa",
    status: "operational",
    service: "TAC",
    responseMs: 52,
    importance: 0.45
  }
];

const BASE_ROUTES: NetworkRoute[] = [
  {
    id: "edge-eu",
    from: "ams-edge",
    to: "dub-core",
    label: "EU Sync",
    gradient: ["#60a5fa", "#f472b6"]
  },
  {
    id: "transatlantic",
    from: "dub-core",
    to: "sfo-core",
    label: "Atlantic Backbone",
    gradient: ["#22d3ee", "#a855f7"]
  },
  {
    id: "apac",
    from: "sin-edge",
    to: "hnd-cache",
    label: "APAC Updates",
    gradient: ["#34d399", "#38bdf8"]
  },
  {
    id: "latam",
    from: "gru-cache",
    to: "sfo-core",
    label: "LATAM Harvest",
    gradient: ["#fb7185", "#f97316"]
  },
  {
    id: "global-south",
    from: "jnb-observer",
    to: "sin-edge",
    label: "Southern Transit",
    gradient: ["#facc15", "#f472b6"]
  },
  {
    id: "oceania",
    from: "syd-core",
    to: "sin-edge",
    label: "Oceania Relay",
    gradient: ["#6366f1", "#22d3ee"]
  },
  {
    id: "indian-ocean",
    from: "bom-edge",
    to: "sin-edge",
    label: "Indian Ocean Mesh",
    gradient: ["#f97316", "#38bdf8"]
  }
];

function useSize<T extends HTMLElement>(ref: React.RefObject<T | null>) {
  const [size, setSize] = useState({ w: 0, h: 0 });
  useEffect(() => {
    if (!ref.current) return;
    const ro = new ResizeObserver(([e]) => {
      const r = e.contentRect;
      setSize({ w: Math.max(300, r.width), h: Math.max(260, r.height) });
    });
    ro.observe(ref.current);
    return () => ro.disconnect();
  }, [ref]);
  return size;
}

export default function Map2D({ offsetTop = 0, userLocation }: Props) {
  const wrapRef = useRef<HTMLDivElement>(null);

  // важно: высоту считаем от уже "подрезанного" контейнера
  const { w, h } = useSize(wrapRef);

  const [worldFc, setWorldFc] = useState<FeatureCollection | null>(null);
  const [hover, setHover] = useState<HoverState | null>(null);
  const [attacks, setAttacks] = useState<Attack[]>([]);
  const centroidsRef = useRef<Record<string, [number, number]>>({});
  const idRef = useRef(0);
  const [nodeDensity, setNodeDensity] = useState(0.9);
  const [speedMultiplier, setSpeedMultiplier] = useState(1);
  const [pan, setPan] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const [isDragging, setIsDragging] = useState(false);
  const dragState = useRef({
    active: false,
    startX: 0,
    startY: 0,
    baseX: 0,
    baseY: 0
  });

  useEffect(() => {
    let alive = true;
    (async () => {
      const topo = await fetch("https://cdn.jsdelivr.net/npm/world-atlas@2/countries-110m.json").then((r) =>
        r.json()
      );
      if (!alive) return;
      const fc = feature(
        topo,
        topo.objects.countries
      ) as unknown as FeatureCollection<Geometry, GeoJsonProperties>;
      setWorldFc(fc);
      const map: Record<string, [number, number]> = {};
      for (const f of fc.features as WorldFeature[]) {
        const nm = f.properties?.NAME || f.properties?.name;
        if (nm) map[nm] = geoCentroid(f as Feature<Polygon | MultiPolygon>) as [number, number];
      }
      centroidsRef.current = map;
    })();
    return () => { alive = false; };
  }, []);

  useEffect(() => {
    const es = mapSSE();
    es.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data);
        if (msg.type === "check.start") {
          const [slat, slng] = msg.data.source.geo;
          const [tlat, tlng] = msg.data.target.geo;
          const a: Attack = { id: idRef.current++, from: [slng, slat], to: [tlng, tlat] };
          setAttacks(s => [...s, a]);
          setTimeout(() => setAttacks(s => s.filter(x => x.id !== a.id)), ATTACK_LIFETIME_MS);
        }
      } catch (_error) {
        // intentionally swallow malformed streaming updates
        void _error;
        return;
      }
    };
    es.onerror = () => {};
    return () => es.close();
  }, []);

  const projection: GeoProjection = useMemo(() => {
    const mercator = geoMercator().center([0, 0]);
    if (w <= 0 || h <= 0) {
      return mercator.translate([0, 0]).scale(1);
    }

    const scaleForWidth = (w / (2 * Math.PI)) * 0.95;
    const scaleForHeight = (h / Math.PI) * 0.95;

    return mercator
      .translate([w / 2, h / 2])
      .scale(Math.min(scaleForWidth, scaleForHeight));
  }, [w, h]);

  const path = useMemo(() => geoPath(projection), [projection]);
  const graticule = useMemo(() => geoGraticule10(), []);

  const sortedNodes = useMemo(() => [...BASE_NODES].sort((a, b) => b.importance - a.importance), []);
  const visibleNodes = useMemo(() => {
    const count = Math.max(3, Math.round(sortedNodes.length * nodeDensity));
    return sortedNodes.slice(0, count);
  }, [sortedNodes, nodeDensity]);

  const nodeLookup = useMemo(() => {
    const map = new Map<string, NetworkNode>();
    for (const node of visibleNodes) map.set(node.id, node);
    return map;
  }, [visibleNodes]);

  const visibleRoutes = useMemo(() => {
    return BASE_ROUTES.filter((route) => nodeLookup.has(route.from) && nodeLookup.has(route.to));
  }, [nodeLookup]);

  const routeStats = useMemo(() => {
    const stats = new Map<string, { jitter: number; packetLoss: number }>();
    for (const route of BASE_ROUTES) {
      const seed = route.id.length;
      const jitter = Math.round((seed % 7) * 2.3 + 3);
      const packetLoss = Number(((seed % 3) * 0.35 + 0.1).toFixed(2));
      stats.set(route.id, { jitter, packetLoss });
    }
    return stats;
  }, []);

  const projectPoint = useMemo(() => {
    return (lng: number, lat: number): [number, number] => {
      return projection([lng, lat]) as [number, number];
    };
  }, [projection]);

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

  const createTrailPath = useMemo(() => {
    return (from: [number, number], to: [number, number]) => {
      const interpolator = geoInterpolate([from[0], from[1]], [to[0], to[1]]);
      const steps = 24;
      const points: string[] = [];
      for (let i = 0; i <= steps; i++) {
        const [lng, lat] = interpolator(i / steps);
        const [x, y] = projectPoint(lng, lat);
        points.push(`${i === 0 ? "M" : "L"} ${x.toFixed(2)},${y.toFixed(2)}`);
      }
      return points.join(" ");
    };
  }, [projectPoint]);

  useEffect(() => {
    if (!wrapRef.current || attacks.length === 0) return;
    const last = attacks[attacks.length - 1];
    const el = wrapRef.current.querySelector(`path[data-attack-id="${last.id}"]`) as SVGPathElement | null;
    if (!el) return;
    const len = el.getTotalLength();
    el.style.strokeDasharray = String(len);
    el.style.strokeDashoffset = String(len);
    el.style.opacity = "1";
    el.getBoundingClientRect();
    el.style.transition = "stroke-dashoffset 1.4s linear, opacity .5s ease .9s";
    el.style.strokeDashoffset = "0";
    setTimeout(() => { el.style.opacity = "0"; }, ATTACK_LIFETIME_MS - 600);
  }, [attacks]);

  const onEnterCountry = (e: ReactMouseEvent<SVGPathElement>, f: WorldFeature) => {
    const name = f.properties?.NAME || f.properties?.name || "";
    const svgRect = (e.currentTarget as SVGPathElement).ownerSVGElement!.getBoundingClientRect();
    setHover({ type: "country", name, x: e.clientX - svgRect.left + 12, y: e.clientY - svgRect.top + 12 });
  };
  const onLeaveCountry = () => setHover(null);
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
        baseY: pan.y
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

  const handleNodeEnter = (
    e: ReactMouseEvent<SVGCircleElement>,
    node: NetworkNode
  ) => {
    const svgRect = (e.currentTarget as SVGElement).ownerSVGElement!.getBoundingClientRect();
    setHover({
      type: "node",
      node,
      x: e.clientX - svgRect.left + 14,
      y: e.clientY - svgRect.top + 14
    });
  };

  const handleRouteEnter = (
    e: ReactMouseEvent<SVGPathElement>,
    route: NetworkRoute
  ) => {
    const svgRect = (e.currentTarget as SVGElement).ownerSVGElement!.getBoundingClientRect();
    setHover({
      type: "route",
      route,
      stats: routeStats.get(route.id) ?? { jitter: 5, packetLoss: 0.2 },
      x: e.clientX - svgRect.left + 16,
      y: e.clientY - svgRect.top + 16
    });
  };

  const handleUserEnter = (
    e: ReactMouseEvent<SVGCircleElement>
  ) => {
    if (!userMarker) return;
    const svgRect = (e.currentTarget as SVGElement).ownerSVGElement?.getBoundingClientRect();
    if (!svgRect) return;
    setHover({
      type: "user",
      title: userMarker.title,
      subtitle: userMarker.subtitle,
      x: e.clientX - svgRect.left + 14,
      y: e.clientY - svgRect.top + 14
    });
  };

  const handleUserLeave = () => setHover(null);

  const userPulseDuration = useMemo(() => `${(4 / speedMultiplier).toFixed(2)}s`, [speedMultiplier]);

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
          background: "radial-gradient(circle at 20% 30%, rgba(34,197,247,0.22), transparent 55%), radial-gradient(circle at 80% 15%, rgba(56,189,248,0.18), transparent 60%), linear-gradient(180deg,#050b1a 0%, #09142a 55%, #03070f 100%)"
        }}
      >
        <style>
          {`
            @keyframes nodePulse {
              0% { transform: scale(0.85); opacity: 0.75; }
              50% { transform: scale(1.1); opacity: 1; }
              100% { transform: scale(0.85); opacity: 0.75; }
            }
            @keyframes routeFlow {
              0% { stroke-dashoffset: 100%; }
              100% { stroke-dashoffset: 0%; }
            }
            .map-node {
              transform-origin: center;
              animation-name: nodePulse;
              animation-iteration-count: infinite;
              will-change: transform, opacity;
            }
            .map-route-trail {
              animation-name: routeFlow;
              animation-iteration-count: infinite;
              animation-timing-function: linear;
              stroke-linecap: round;
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
            {visibleRoutes.map((route) => {
              const fromNode = nodeLookup.get(route.from)!;
              const toNode = nodeLookup.get(route.to)!;
              const [sx, sy] = projectPoint(fromNode.lng, fromNode.lat);
              const [tx, ty] = projectPoint(toNode.lng, toNode.lat);
              return (
                <linearGradient
                  key={route.id}
                  id={`route-gradient-${route.id}`}
                  gradientUnits="userSpaceOnUse"
                  x1={sx}
                  y1={sy}
                  x2={tx}
                  y2={ty}
                >
                  <stop offset="0%" stopColor={route.gradient[0]} />
                  <stop offset="100%" stopColor={route.gradient[1]} />
                </linearGradient>
              );
            })}
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

            {visibleRoutes.map((route) => {
              const from = nodeLookup.get(route.from)!;
              const to = nodeLookup.get(route.to)!;
              const pathD = createArcPath([from.lng, from.lat], [to.lng, to.lat]);
              const trailD = createTrailPath([from.lng, from.lat], [to.lng, to.lat]);
              const dashArray = 180;
              const duration = `${(8 / speedMultiplier).toFixed(2)}s`;
              return (
                <g key={route.id}>
                  <path
                    d={trailD}
                    fill="none"
                    stroke={`url(#route-gradient-${route.id})`}
                    strokeWidth={1.4}
                    strokeDasharray={`${dashArray}`}
                    className="map-route-trail"
                    style={{
                      animationDuration: duration,
                      strokeOpacity: 0.55,
                      strokeDashoffset: dashArray,
                      cursor: isDragging ? "grabbing" : "pointer"
                    }}
                    onMouseEnter={(e) => handleRouteEnter(e, route)}
                    onMouseLeave={() => setHover(null)}
                  />
                  <path
                    d={pathD}
                    fill="none"
                    stroke={`url(#route-gradient-${route.id})`}
                    strokeWidth={2.4}
                    strokeOpacity={0.85}
                    filter="url(#landHalo)"
                    style={{ cursor: isDragging ? "grabbing" : "pointer" }}
                    onMouseEnter={(e) => handleRouteEnter(e, route)}
                    onMouseLeave={() => setHover(null)}
                  />
                </g>
              );
            })}

            {visibleNodes.map((node, idx) => {
              const [x, y] = projectPoint(node.lng, node.lat);
              const pulseDuration = `${(4.5 / (speedMultiplier * (0.6 + node.importance))).toFixed(2)}s`;
              const markerSize = 4 + node.importance * 4;
              return (
                <g key={node.id} transform={`translate(${x},${y})`}>
                  <circle
                    r={markerSize * 1.9}
                    fill={`rgba(56,189,248,${0.12 + idx * 0.03})`}
                    opacity={0.65}
                  />
                  <circle
                    r={markerSize * 1.3}
                    fill="rgba(14,165,233,0.35)"
                    className="map-node"
                    style={{ animationDuration: pulseDuration, animationDelay: `${idx * 0.25}s` }}
                  />
                  <circle
                    r={markerSize}
                    fill="#e0f2fe"
                    stroke="#38bdf8"
                    strokeWidth={1.2}
                    className="map-node"
                    style={{
                      animationDuration: pulseDuration,
                      animationDelay: `${idx * 0.25}s`,
                      cursor: isDragging ? "grabbing" : "pointer"
                    }}
                    onMouseEnter={(e) => handleNodeEnter(e, node)}
                    onMouseLeave={() => setHover(null)}
                  />
                </g>
              );
            })}

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
                    cursor: isDragging ? "grabbing" : "pointer"
                  }}
                  onMouseEnter={handleUserEnter}
                  onMouseLeave={handleUserLeave}
                />
                <circle r={3.2} fill={USER_MARKER_STROKE} pointerEvents="none" />
              </g>
            )}

            {attacks.map((a) => {
              const d = createArcPath(a.from, a.to);
              return (
                <path
                  key={a.id}
                  data-attack-id={a.id}
                  d={d}
                  fill="none"
                  stroke={ATTACK_COLOR}
                  strokeWidth={ATTACK_WIDTH}
                  strokeLinecap="round"
                  style={{ opacity: 0 }}
                />
              );
            })}
          </g>
        </svg>

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
              maxWidth: 220
            }}
          >
            {hover.type === "country" && <div>{hover.name}</div>}
            {hover.type === "node" && (
              <div style={{ display: "grid", gap: 4 }}>
                <div style={{ fontWeight: 600 }}>{hover.node.name}</div>
                <div style={{ color: "#94a3b8" }}>{hover.node.country}</div>
                <div>
                  <span style={{ color: "#38bdf8" }}>Service:</span> {hover.node.service}
                </div>
                <div>
                  <span style={{ color: "#38bdf8" }}>Status:</span> {hover.node.status}
                </div>
                <div>
                  <span style={{ color: "#38bdf8" }}>Response:</span> {hover.node.responseMs} ms
                </div>
              </div>
            )}
            {hover.type === "route" && (
              <div style={{ display: "grid", gap: 4 }}>
                <div style={{ fontWeight: 600 }}>{hover.route.label}</div>
                <div style={{ color: "#94a3b8" }}>
                  {nodeLookup.get(hover.route.from)?.name} → {nodeLookup.get(hover.route.to)?.name}
                </div>
                <div>
                  <span style={{ color: "#38bdf8" }}>Jitter:</span> {hover.stats.jitter} ms
                </div>
                <div>
                  <span style={{ color: "#38bdf8" }}>Packet loss:</span> {hover.stats.packetLoss}%
                </div>
              </div>
            )}
            {hover.type === "user" && (
              <div style={{ display: "grid", gap: 4 }}>
                <div style={{ fontWeight: 600 }}>{hover.title}</div>
                {hover.subtitle && <div style={{ color: "#94a3b8" }}>{hover.subtitle}</div>}
              </div>
            )}
          </div>
        )}

        <div
          style={{
            position: "absolute",
            right: 16,
            bottom: 16,
            width: 240,
            background: "rgba(3,7,18,0.82)",
            border: "1px solid rgba(56,189,248,0.25)",
            borderRadius: 12,
            padding: "12px 16px",
            color: "#cbd5f5",
            boxShadow: "0 16px 40px rgba(2,6,23,0.65)",
            display: "grid",
            gap: 12
          }}
        >
          <div style={{ fontWeight: 600, fontSize: 13, letterSpacing: "0.02em" }}>Карта трафика</div>
          <label style={{ display: "grid", gap: 6, fontSize: 12 }}>
            <span style={{ color: "#94a3b8" }}>Плотность узлов ({visibleNodes.length}/{BASE_NODES.length})</span>
            <input
              type="range"
              min={0.3}
              max={1}
              step={0.1}
              value={nodeDensity}
              onChange={(e) => setNodeDensity(Number(e.target.value))}
              style={{ accentColor: "#38bdf8" }}
            />
          </label>
          <label style={{ display: "grid", gap: 6, fontSize: 12 }}>
            <span style={{ color: "#94a3b8" }}>Скорость анимации ({speedMultiplier.toFixed(1)}x)</span>
            <input
              type="range"
              min={0.6}
              max={2.2}
              step={0.1}
              value={speedMultiplier}
              onChange={(e) => setSpeedMultiplier(Number(e.target.value))}
              style={{ accentColor: "#f472b6" }}
            />
          </label>
        </div>
      </div>
    </div>
  );
}
