import { useEffect, useMemo, useRef, useState } from "react";
import Globe, { GlobeMethods } from "react-globe.gl";
import { feature } from "topojson-client";
import { geoCentroid, geoArea } from "d3-geo";
import type { FeatureCollection, Feature, MultiPolygon, Polygon } from "geojson";
import { mapSSE } from "../lib/api";

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

/* ---------- типы стран/подписей ---------- */
type CountryFeature = Feature<
  Polygon | MultiPolygon,
  { name?: string; ADMIN?: string; name_long?: string }
>;

type CountryLabel = {
  id: number;
  name: string;
  lat: number;
  lng: number;
  area: number; // географическая площадь (в стерадианах) — для оценки размера
};

/* ---------- цвета ---------- */
const SEA_GLOW = "#63a3ff";
const LAND_CAP = "#0f3b37"; // тёмно-зелёный
const LAND_SIDE = "rgba(15, 23, 42, .92)";
const LAND_STROKE = "rgba(56, 189, 248, .35)";
const LABEL_COLOR = "rgba(226,232,240,.92)";

export default function MapGlobe() {
  const globeRef = useRef<GlobeMethods>();
  const camAltRef = useRef(1.6);         // текущая высота камеры
  const [tick, setTick] = useState(0);   // лёгкий триггер пересчёта LOD

  const [polygons, setPolygons] = useState<CountryFeature[]>([]);
  const [labelsAll, setLabelsAll] = useState<CountryLabel[]>([]);
  const [arcs, setArcs] = useState<Arc[]>([]);
  const [points, setPoints] = useState<Dot[]>([]);

  /* ---------- загрузка карт и расчёт подписей ---------- */
  useEffect(() => {
    // стартовая точка обзора
    globeRef.current?.pointOfView({ lat: 30, lng: 20, altitude: 1.6 }, 1200);

    fetch("https://unpkg.com/world-atlas@2.0.2/countries-110m.json")
      .then((r) => r.json())
      .then((topo: any) => {
        const fc = feature(
          topo,
          topo.objects.countries
        ) as FeatureCollection<Polygon | MultiPolygon, any>;

        const feats = fc.features as CountryFeature[];
        setPolygons(feats);

        // готовим подписи
        const prepared = feats
          .map((f, i) => {
            const props = f.properties || {};
            const rawName: string =
              (props.name as string) ||
              (props.ADMIN as string) ||
              (props.name_long as string) ||
              "";

            if (!rawName) return null;

            // d3-гео: centroid => [lng, lat]
            const c = geoCentroid(f as any);
            const area = geoArea(f as any);

            return {
              id: i,
              name: rawName,
              lng: c[0],
              lat: c[1],
              area,
            } as CountryLabel;
          })
          .filter(Boolean) as CountryLabel[];

        // отсечь совсем мелкие страны и отсортировать по площади
        const labels = prepared
          .filter((d) => d.area > 1e-4) // микроскопические острова
          .sort((a, b) => b.area - a.area);

        setLabelsAll(labels);
      })
      .catch(() => {
        // оффлайн — просто без подписей
      });
  }, []);

  /* ---------- простая SSE-логика (оставил как было) ---------- */
  useEffect(() => {
    const es = mapSSE();
    es.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data);
        switch (msg.type) {
          case "agent.online": {
            const [lat, lng] = msg.data.geo;
            setPoints((p) => [
              ...p.filter((x) => x.id !== `ag:${msg.data.id}`),
              {
                id: `ag:${msg.data.id}`,
                lat,
                lng,
                color: "#59a5ff",
                size: 0.4,
                label: `agent ${msg.data.ip}`,
              },
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
                color: "#9b5de5",
                status: "running",
              },
            ]);
            setPoints((p) => [
              ...p,
              { id: `s:${check_id}`, lat: slat, lng: slng, color: "#9b5de5", size: 0.5, label: "agent" },
              { id: `t:${check_id}`, lat: tlat, lng: tlng, color: "#ffe66d", size: 0.5, label: target.host },
            ]);
            break;
          }
          case "check.done": {
            const ok = !!msg.data.ok;
            setArcs((as) =>
              as.map((a) =>
                a.id === msg.data.check_id
                  ? { ...a, color: ok ? "#4ade80" : "#ef4444", status: ok ? "ok" : "fail" }
                  : a
              )
            );
            setTimeout(() => {
              setPoints((p) =>
                p.filter(
                  (x) => !x.id.startsWith(`s:${msg.data.check_id}`) && !x.id.startsWith(`t:${msg.data.check_id}`)
                )
              );
            }, 3000);
            break;
          }
          default:
            break;
        }
      } catch {
        /* ignore */
      }
    };
    es.onerror = () => {};
    return () => es.close();
  }, []);

  /* ---------- динамический LOD для подписей (без лагов) ---------- */
  const visibleLabels = useMemo(() => {
    const alt = camAltRef.current;

    // чем выше — тем меньше подписей
    let take = 0;
    if (alt > 2.2) take = 20;
    else if (alt > 1.7) take = 45;
    else if (alt > 1.4) take = 80;
    else if (alt > 1.2) take = 120;
    else take = 160;

    return labelsAll.slice(0, take);
  }, [labelsAll, tick]);

  // без setState на каждый zoom — только лёгкий "ping"
  const handleZoom = () => {
    const a = globeRef.current?.pointOfView()?.altitude;
    if (typeof a === "number") {
      camAltRef.current = a;
      // лёгкое обновление для пересчёта visibleLabels (раз в ~150мс)
      if (!zoomRaf.current) {
        zoomRaf.current = requestAnimationFrame(() => {
          zoomRaf.current = null;
          setTick((t) => t + 1);
        });
      }
    }
  };
  const zoomRaf = useRef<number | null>(null);

  /* ---------- размер шрифта от площади (без тяжёлых вычислений) ---------- */
  const labelSize = (d: CountryLabel) => {
    const a = d.area; // 0..~0.2
    if (a > 0.08) return 1.35; // крупные (Россия, Канада, Китай)
    if (a > 0.05) return 1.05;
    if (a > 0.02) return 0.85;
    if (a > 0.01) return 0.72;
    if (a > 0.005) return 0.64;
    return 0.56;
  };

  return (
    <div className="h-[70vh] md:h-[80vh] w-full">
      <Globe
        ref={globeRef}
        backgroundColor="rgba(0,0,0,0)"

        /* атмосфера и фон */
        showAtmosphere
        atmosphereColor={SEA_GLOW}
        atmosphereAltitude={0.25}
        globeImageUrl="//unpkg.com/three-globe/example/img/earth-dark.jpg"
        bumpImageUrl="//unpkg.com/three-globe/example/img/earth-topology.png"

        /* материки */
        polygonsData={polygons}
        polygonAltitude={0.02}
        polygonCapColor={() => LAND_CAP}
        polygonSideColor={() => LAND_SIDE}
        polygonStrokeColor={() => LAND_STROKE}
        polygonsTransitionDuration={0}

        /* подписи стран (точно по центроиду, чуть приподняты) */
        labelsData={visibleLabels}
        labelLat={(d: CountryLabel) => d.lat}
        labelLng={(d: CountryLabel) => d.lng}
        labelText={(d: CountryLabel) => d.name}
        labelSize={labelSize}
        labelColor={() => LABEL_COLOR}
        labelAltitude={0.03}
        labelResolution={2}
        labelIncludeDot={false}

        /* точки (агенты/цели) */
        pointsData={points}
        pointLat={(d: Dot) => d.lat}
        pointLng={(d: Dot) => d.lng}
        pointAltitude={0.01}
        pointColor={(d: Dot) => d.color}
        pointRadius={(d: Dot) => d.size}
        pointLabel={(d: Dot) => d.label || ""}

        /* дуги */
        arcsData={arcs}
        arcStartLat={(d: Arc) => d.startLat}
        arcStartLng={(d: Arc) => d.startLng}
        arcEndLat={(d: Arc) => d.endLat}
        arcEndLng={(d: Arc) => d.endLng}
        arcColor={(d: Arc) => d.color}
        arcAltitude={0.25}
        arcStroke={0.6}
        arcDashLength={0.45}
        arcDashGap={0.2}
        arcDashAnimateTime={1600}

        /* интерактив */
        enablePointerInteraction={true}
        onZoom={handleZoom}
      />
    </div>
  );
}
