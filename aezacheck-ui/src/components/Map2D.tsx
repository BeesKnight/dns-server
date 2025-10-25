// src/components/Map2D.tsx
import { useEffect, useMemo, useRef, useState } from "react";
import {
  geoMercator, geoPath, geoGraticule10, geoCentroid, type GeoProjection
} from "d3-geo";
import { feature } from "topojson-client";
import type { FeatureCollection, Feature, MultiPolygon, Polygon } from "geojson";
import { mapSSE } from "../lib/api";

/* --- стиль фона --- */
const BG_GRADIENT =
  "radial-gradient(1000px 600px at 30% 55%, rgba(32,102,208,.25), rgba(0,0,0,0) 70%)," +
  "radial-gradient(800px 500px at 85% 8%, rgba(26,58,120,.18), rgba(0,0,0,0) 65%)," +
  "linear-gradient(180deg,#0a1124 0%, #09142a 55%, #0a1227 100%)";

const BORDER_STROKE = "rgba(56,189,248,.35)";
const LAND_FILL_A = "#0f3b37";
const LAND_FILL_B = "#0f3b37";
const HOVER_FILL  = "#13524c";
const GRID_STROKE = "rgba(56,189,248,.10)";
const ATTACK_COLOR = "#ff4bd6";
const ATTACK_WIDTH = 2.2;
const ATTACK_LIFETIME_MS = 2000;

type WorldFeature = Feature<Polygon | MultiPolygon, { NAME?: string; name?: string }>;
type Attack = { id: number; from: [number, number]; to: [number, number]; };

type Props = {
  /** на сколько пикселей опустить карту вниз */
  offsetTop?: number;
};

function useSize(ref: React.RefObject<HTMLElement>) {
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

export default function Map2D({ offsetTop = 0 }: Props) {
  const wrapRef = useRef<HTMLDivElement>(null);

  // важно: высоту считаем от уже "подрезанного" контейнера
  const { w, h } = useSize(wrapRef);

  const [worldFc, setWorldFc] = useState<FeatureCollection | null>(null);
  const [hover, setHover] = useState<{ name: string; x: number; y: number } | null>(null);
  const [attacks, setAttacks] = useState<Attack[]>([]);
  const centroidsRef = useRef<Record<string, [number, number]>>({});
  const idRef = useRef(0);

  useEffect(() => {
    let alive = true;
    (async () => {
      const topo = await fetch("https://cdn.jsdelivr.net/npm/world-atlas@2/countries-110m.json").then(r => r.json());
      if (!alive) return;
      const fc = feature(topo, topo.objects.countries) as FeatureCollection;
      setWorldFc(fc);
      const map: Record<string, [number, number]> = {};
      for (const f of fc.features as WorldFeature[]) {
        const nm = f.properties?.NAME || f.properties?.name;
        if (nm) map[nm] = geoCentroid(f as any) as [number, number];
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
      } catch {}
    };
    es.onerror = () => {};
    return () => es.close();
  }, []);

  const projection: GeoProjection = useMemo(() => {
    const p = geoMercator().translate([w / 2, h / 2]);
    const s = (w / (2 * Math.PI)) * 0.95 * (h / (w || 1) > 0.45 ? 1 : 0.9);
    return p.scale(s);
  }, [w, h]);

  const path = useMemo(() => geoPath(projection), [projection]);
  const graticule = useMemo(() => geoGraticule10(), []);

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

  const onEnterCountry = (e: React.MouseEvent, f: WorldFeature) => {
    const name = f.properties?.NAME || f.properties?.name || "";
    const svgRect = (e.currentTarget as SVGPathElement).ownerSVGElement!.getBoundingClientRect();
    setHover({ name, x: e.clientX - svgRect.left + 12, y: e.clientY - svgRect.top + 12 });
  };
  const onLeaveCountry = () => setHover(null);
  const onSvgMove = (e: React.MouseEvent<SVGSVGElement>) => {
    if (!hover) return;
    const r = e.currentTarget.getBoundingClientRect();
    setHover({ ...hover, x: e.clientX - r.left + 12, y: e.clientY - r.top + 12 });
  };

  return (
    <div
      // <<<<<< вот эти две строки опускают карту и сохраняют высоту
      style={{ marginTop: offsetTop, height: `calc(100% - ${offsetTop}px)` }}
      className="w-full"
    >
      <div
        ref={wrapRef}
        style={{
          position: "relative",
          width: "100%",
          height: "100%",
          background: BG_GRADIENT,
          overflow: "hidden",
          borderRadius: 12
        }}
      >
        <svg width={w} height={h} onMouseMove={onSvgMove}>
          <path d={path(graticule) || ""} fill="none" stroke={GRID_STROKE} strokeWidth={0.6} />

          {worldFc &&
            (worldFc.features as WorldFeature[]).map((f, i) => (
              <path
                key={(f.id as string) || i}
                d={path(f) || ""}
                fill={i % 2 === 0 ? LAND_FILL_A : LAND_FILL_B}
                stroke={BORDER_STROKE}
                strokeWidth={0.6}
                style={{ transition: "fill .15s ease" }}
                onMouseEnter={(e) => onEnterCountry(e, f)}
                onMouseLeave={onLeaveCountry}
                onMouseOver={(e) => ((e.currentTarget as SVGPathElement).style.fill = HOVER_FILL)}
                onMouseOut={(e) =>
                  ((e.currentTarget as SVGPathElement).style.fill = i % 2 === 0 ? LAND_FILL_A : LAND_FILL_B)
                }
              />
            ))}

          {attacks.map((a) => {
            const line = { type: "LineString" as const, coordinates: [a.from, a.to] };
            return (
              <path
                key={a.id}
                data-attack-id={a.id}
                d={path(line) || ""}
                fill="none"
                stroke={ATTACK_COLOR}
                strokeWidth={ATTACK_WIDTH}
                strokeLinecap="round"
                style={{ opacity: 0 }}
              />
            );
          })}
        </svg>

        {hover && (
          <div
            style={{
              position: "absolute",
              left: hover.x,
              top: hover.y,
              pointerEvents: "none",
              background: "rgba(6,7,12,.85)",
              color: "#e2e8f0",
              padding: "6px 10px",
              borderRadius: 6,
              fontSize: 12,
              border: "1px solid rgba(56,189,248,.25)",
              boxShadow: "0 6px 18px rgba(0,0,0,.35)"
            }}
          >
            {hover.name}
          </div>
        )}
      </div>
    </div>
  );
}
