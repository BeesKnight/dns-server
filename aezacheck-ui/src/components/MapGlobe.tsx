import { useEffect, useMemo, useRef, useState } from "react";
import Globe from "react-globe.gl";
import { mapSSE } from "../lib/api";
import { feature } from "topojson-client";
import { geoCentroid, geoArea } from "d3-geo";

/* ================= Types ================= */

type MapEvt =
  | { type: "agent.online"; ts: string; data: { id: string; ip: string; geo: [number, number] } }
  | {
      type: "check.start";
      ts: string;
      data: {
        check_id: string;
        source: { id: string; ip: string; geo: [number, number] };
        target: { host: string; geo: [number, number] };
      };
    }
  | { type: "check.done"; ts: string; data: { check_id: string; ok: boolean; ms?: number } }
  | { type: "check.result"; ts: string; data: { check_id: string; kind: string; ok: boolean } };

type Arc = {
  id: string;
  startLat: number;
  startLng: number;
  endLat: number;
  endLng: number;
  color: string;
  status: "running" | "ok" | "fail";
  greatCircle: boolean;
};

type Dot = { id: string; lat: number; lng: number; color: string; size: number; label?: string };

type CountryLabel = {
  id: string;
  name: string;
  lat: number;
  lng: number;
  area: number; // для «важности»
};

/* ================= Colors ================= */

const BLUE = "#59a5ff";
const PURPLE = "#9b5de5";
const GREEN = "#4ade80";
const RED = "#ef4444";

// оформление земли:
const OCEAN = "#071a2f"; // тёмно-синий «вода»
const LAND_CAP = "#0d3b33"; // тёмно-зелёная «крышка» материков
const LAND_SIDE = "rgba(12, 53, 44, 0.95)"; // боковины
const LAND_STROKE = "rgba(148,163,184,0.25)"; // контур
const GLOW = "#2b6ef3"; // свечение

/* ============== Вспомогалки ============== */

function clamp(n: number, a: number, b: number) {
  return Math.max(a, Math.min(b, n));
}

/* ============== Компонент ================ */

export default function MapGlobe() {
  const globeRef = useRef<any>(null);

  const [arcs, setArcs] = useState<Arc[]>([]);
  const [points, setPoints] = useState<Dot[]>([]);
  const [polygons, setPolygons] = useState<any[]>([]);
  const [labels, setLabels] = useState<CountryLabel[]>([]);
  const [altitude, setAltitude] = useState(2.0); // текущее приближение

  /* ---- камера + загрузка стран ---- */
  useEffect(() => {
    // стартовая камера
    globeRef.current?.pointOfView({ lat: 48.5, lng: 6, altitude: 1.9 }, 1200);

    // страны (topojson -> geojson)
    fetch("https://unpkg.com/world-atlas@2.0.2/countries-110m.json")
      .then((r) => r.json())
      .then((topo) => {
        const geo = feature(topo, (topo as any).objects.countries) as any;
        const feats = geo.features ?? [];
        setPolygons(feats);

        // Предрасчёт подписей (центроид + площадь)
        const prepared: CountryLabel[] = feats
          .map((f: any) => {
            const [lng, lat] = geoCentroid(f);
            const area = geoArea(f); // ради сортировки/фильтрации
            const name =
              f.properties?.name ||
              f.properties?.NAME ||
              f.properties?.ADMIN ||
              f.id ||
              "Unknown";
            return { id: String(f.id ?? name), name, lat, lng, area };
          })
          // иногда встречаются крошечные «страны»-островки без имени
          .filter((c: CountryLabel) => !!c.name && Number.isFinite(c.lat) && Number.isFinite(c.lng));

        setLabels(prepared);
      })
      .catch(() => {
        // оффлайн — просто без подписей/полигонов
      });
  }, []);

  /* ---- отслеживаем высоту камеры (для количества подписей) ---- */
  useEffect(() => {
    const id = setInterval(() => {
      try {
        const pov = globeRef.current?.pointOfView() || {};
        if (typeof pov.altitude === "number") setAltitude(pov.altitude);
      } catch {}
    }, 250);
    return () => clearInterval(id);
  }, []);

  /* ---- SSE события карты ---- */
  useEffect(() => {
    const es = mapSSE();
    es.onmessage = (ev) => {
      try {
        const msg: MapEvt = JSON.parse(ev.data);
        switch (msg.type) {
          case "agent.online": {
            const [lat, lng] = msg.data.geo;
            setPoints((p) => [
              ...p.filter((x) => x.id !== `ag:${msg.data.id}`),
              { id: `ag:${msg.data.id}`, lat, lng, color: BLUE, size: 0.4, label: `agent ${msg.data.ip}` },
            ]);
            break;
          }
          case "check.start": {
            const { source, target, check_id } = msg.data;
            const [slat, slng] = source.geo;
            const [tlat, tlng] = target.geo;
            setArcs((as) => [
              ...as,
              {
                id: check_id,
                startLat: slat,
                startLng: slng,
                endLat: tlat,
                endLng: tlng,
                color: PURPLE,
                greatCircle: true,
                status: "running",
              },
            ]);
            setPoints((p) => [
              ...p,
              { id: `s:${check_id}`, lat: slat, lng: slng, color: PURPLE, size: 0.5, label: "agent" },
              { id: `t:${check_id}`, lat: tlat, lng: tlng, color: "#ffe66d", size: 0.5, label: target.host },
            ]);
            break;
          }
          case "check.done": {
            setArcs((as) =>
              as.map((a) =>
                a.id === msg.data.check_id
                  ? { ...a, color: msg.data.ok ? GREEN : RED, status: msg.data.ok ? "ok" : "fail" }
                  : a
              )
            );
            setTimeout(() => {
              setPoints((p) => p.filter((x) => x.id !== `s:${msg.data.check_id}` && x.id !== `t:${msg.data.check_id}`));
            }, 3000);
            break;
          }
        }
      } catch {
        /* ignore */
      }
    };
    es.onerror = () => {};
    return () => es.close();
  }, []);

  /* ---- арки ограничиваем, чтобы не «засорять» GPU ---- */
  const arcsData = useMemo(() => arcs.slice(-120), [arcs]);

  /* ---- вычисляем видимые подписи с учётом зума ---- */
  const visibleLabels = useMemo(() => {
    // сколько подписей показывать при текущем зуме
    // меньше altitude -> ближе к земле -> можно больше подписей
    // простая эмпирика
    const targetCount = clamp(Math.round(60 / Math.pow(altitude, 0.6)), 8, 80);

    // минимальная площадь — отсеиваем микрокрошки
    const minArea = clamp(0.000002 / Math.pow(altitude, 1.3), 0.0000008, 0.00002);

    return labels
      .filter((l) => l.area >= minArea)
      .sort((a, b) => b.area - a.area)
      .slice(0, targetCount);
  }, [labels, altitude]);

  // размер шрифта для подписи под текущим зумом
  const sizeFor = (d: CountryLabel) => {
    // базовый от площади, слегка от зума
    const base = 0.6 + Math.log10(1e6 * d.area + 1) * 0.9;
    const z = clamp(2.6 - altitude, 0.9, 2.4);
    return clamp(base * z, 0.7, 3.2);
  };

  return (
    <div className="relative w-full h-[70vh] md:h-[80vh]">
      {/* мягкое синеватое свечение под землёй */}
      <div className="pointer-events-none absolute inset-0 rounded-full"
           style={{
             left: "6%",
             right: "56%",
             top: "6%",
             bottom: "10%",
             filter: "blur(60px)",
             background: `radial-gradient(60% 60% at 50% 50%, ${GLOW}40 0%, transparent 70%)`,
           }}
      />

      <Globe
        ref={globeRef}
        // отдаём рендереру «экономный» приоритет — часто помогает на ноутбуках
        rendererConfig={{ antialias: true, powerPreference: "high-performance" } as any}

        backgroundColor="rgba(0,0,0,0)"
        showAtmosphere
        atmosphereColor="#6ea8fe"
        atmosphereAltitude={0.25}

        // берём нейтральную тёмную текстуру океана,
        // а цвет воды задаём самостоятельно, чтобы не было «чёрной дырки»
        globeImageUrl="//unpkg.com/three-globe/example/img/earth-dark.jpg"
        bumpImageUrl="//unpkg.com/three-globe/example/img/earth-topology.png"

        // заливаем «воду» (фон сферы) темно-синим
        globeMaterialOptions={{ color: OCEAN } as any}

        /* ==== Выпирающие материки ==== */
        polygonsData={polygons}
        polygonAltitude={0.012}
        polygonCapColor={() => LAND_CAP}
        polygonSideColor={() => LAND_SIDE}
        polygonStrokeColor={() => LAND_STROKE}
        polygonsTransitionDuration={0}

        /* ==== Точки (агенты/цели) ==== */
        pointsData={points}
        pointLat={(d: any) => (d as Dot).lat}
        pointLng={(d: any) => (d as Dot).lng}
        pointAltitude={0.01}
        pointColor={(d: any) => (d as Dot).color}
        pointRadius={(d: any) => (d as Dot).size}
        pointLabel={(d: any) => (d as Dot).label || ""}

        /* ==== Дуги агент → цель ==== */
        arcsData={arcsData}
        arcStartLat={(d: any) => (d as Arc).startLat}
        arcStartLng={(d: any) => (d as Arc).startLng}
        arcEndLat={(d: any) => (d as Arc).endLat}
        arcEndLng={(d: any) => (d as Arc).endLng}
        arcColor={(d: any) => (d as Arc).color}
        arcAltitude={0.25}
        arcStroke={0.6}
        arcDashLength={0.45}
        arcDashGap={0.2}
        arcDashAnimateTime={1600}

        /* ==== Подписи стран (оптимизированные) ==== */
        labelsData={visibleLabels as any}
        labelLat={(d: any) => (d as CountryLabel).lat}
        labelLng={(d: any) => (d as CountryLabel).lng}
        labelText={(d: any) => (d as CountryLabel).name}
        labelSize={(d: any) => sizeFor(d as CountryLabel)}
        labelColor={() => "rgba(226, 232, 240, 0.92)"} // slate-200
        labelAltitude={0.02}
        labelResolution={2}
        labelIncludeDot={false}
        labelDotRadius={0}
        labelsTransitionDuration={0}
      />
    </div>
  );
}
