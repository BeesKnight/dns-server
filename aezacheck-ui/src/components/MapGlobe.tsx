import { useEffect, useMemo, useRef, useState } from "react";
import Globe, { GlobeMethods } from "react-globe.gl";
import { feature } from "topojson-client";
import { geoCentroid, geoArea } from "d3-geo";
import type { FeatureCollection, Feature, MultiPolygon, Polygon } from "geojson";
import { mapSSE } from "../lib/api";
import type { Arc, Dot, WorkerCommand, WorkerResponse } from "../types/globe";

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

const FPS_LOW_THRESHOLD = 28;
const FPS_HIGH_THRESHOLD = 34;

export default function MapGlobe() {
  const globeRef = useRef<GlobeMethods>();
  const camAltRef = useRef(1.6); // текущая высота камеры
  const [tick, setTick] = useState(0); // лёгкий триггер пересчёта LOD

  const [polygons, setPolygons] = useState<CountryFeature[]>([]);
  const [labelsAll, setLabelsAll] = useState<CountryLabel[]>([]);
  const [arcs, setArcs] = useState<Arc[]>([]);
  const [points, setPoints] = useState<Dot[]>([]);
  const [fps, setFps] = useState(60);
  const [performanceMode, setPerformanceMode] = useState(false);

  const workerRef = useRef<Worker | null>(null);
  const arcCacheRef = useRef(new Map<string, Arc>());
  const pointCacheRef = useRef(new Map<string, Dot>());
  const zoomRaf = useRef<number | null>(null);
  const performanceModeRef = useRef(false);

  /* ---------- загрузка карт и расчёт подписей ---------- */
  useEffect(() => {
    // стартовая точка обзора
    globeRef.current?.pointOfView({ lat: 30, lng: 20, altitude: 1.6 }, 1200);

    const worker = new Worker(new URL("../workers/globeWorker.ts", import.meta.url));
    workerRef.current = worker;

    const applyPointUpdates = (updates: Dot[]) => {
      if (!updates.length) return;
      setPoints((prev) => {
        if (!prev.length && !updates.length) {
          return prev;
        }
        const updateMap = new Map(updates.map((item) => [item.id, item]));
        let changed = false;
        const next = prev.map((item) => {
          const candidate = updateMap.get(item.id);
          if (!candidate) return item;
          const cached = pointCacheRef.current.get(item.id);
          const sameAsCurrent = shallowDotEqual(item, candidate);
          if ((cached && shallowDotEqual(cached, candidate)) || sameAsCurrent) {
            pointCacheRef.current.set(candidate.id, candidate);
            updateMap.delete(item.id);
            return item;
          }
          pointCacheRef.current.set(candidate.id, candidate);
          updateMap.delete(item.id);
          changed = true;
          return candidate;
        });

        if (updateMap.size) {
          changed = true;
          updateMap.forEach((value) => {
            pointCacheRef.current.set(value.id, value);
            next.push(value);
          });
        }
        return changed ? next : prev;
      });
    };

    const removePoints = (ids: string[]) => {
      if (!ids.length) return;
      const removeSet = new Set(ids);
      setPoints((prev) => {
        if (!prev.length) return prev;
        let changed = false;
        const next = prev.filter((item) => {
          if (removeSet.has(item.id)) {
            pointCacheRef.current.delete(item.id);
            changed = true;
            return false;
          }
          return true;
        });
        return changed ? next : prev;
      });
    };

    const applyArcUpdates = (updates: Arc[]) => {
      if (!updates.length) return;
      setArcs((prev) => {
        const updateMap = new Map(updates.map((item) => [item.id, item]));
        let changed = false;
        const next = prev.map((item) => {
          const candidate = updateMap.get(item.id);
          if (!candidate) return item;
          const cached = arcCacheRef.current.get(item.id);
          const sameAsCurrent = shallowArcEqual(item, candidate);
          if ((cached && shallowArcEqual(cached, candidate)) || sameAsCurrent) {
            arcCacheRef.current.set(candidate.id, candidate);
            updateMap.delete(item.id);
            return item;
          }
          arcCacheRef.current.set(candidate.id, candidate);
          updateMap.delete(item.id);
          changed = true;
          return candidate;
        });

        if (updateMap.size) {
          changed = true;
          updateMap.forEach((value) => {
            arcCacheRef.current.set(value.id, value);
            next.push(value);
          });
        }
        return changed ? next : prev;
      });
    };

    const removeArcs = (ids: string[]) => {
      if (!ids.length) return;
      const removeSet = new Set(ids);
      setArcs((prev) => {
        if (!prev.length) return prev;
        let changed = false;
        const next = prev.filter((item) => {
          if (removeSet.has(item.id)) {
            arcCacheRef.current.delete(item.id);
            changed = true;
            return false;
          }
          return true;
        });
        return changed ? next : prev;
      });
    };

    const handleWorkerMessage = (event: MessageEvent<WorkerResponse>) => {
      const payload = event.data;
      if (!payload) return;
      switch (payload.type) {
        case "points:add":
        case "points:update":
          applyPointUpdates(payload.points);
          break;
        case "points:remove":
          removePoints(payload.ids);
          break;
        case "arcs:add":
        case "arcs:update":
          applyArcUpdates(payload.arcs);
          break;
        case "arcs:remove":
          removeArcs(payload.ids);
          break;
        default:
          break;
      }
    };

    worker.addEventListener("message", handleWorkerMessage);

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

    return () => {
      worker.removeEventListener("message", handleWorkerMessage);
      worker.terminate();
      workerRef.current = null;
    };
  }, []);

  /* ---------- простая SSE-логика (оставил как было) ---------- */
  useEffect(() => {
    const es = mapSSE();
    es.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data) as WorkerCommand;
        workerRef.current?.postMessage(msg);
        if (msg.type === "check.done") {
          const checkId = msg.data.check_id;
          setTimeout(() => {
            workerRef.current?.postMessage({
              type: "points.remove",
              data: { ids: [`s:${checkId}`, `t:${checkId}`] },
            } satisfies WorkerCommand);
          }, 3000);
        }
      } catch {
        /* ignore */
      }
    };
    es.onerror = () => {};
    return () => es.close();
  }, []);

  useEffect(() => {
    let rafId: number;
    let frameCount = 0;
    let lastTime = performance.now();

    const loop = (time: number) => {
      frameCount += 1;
      const delta = time - lastTime;
      if (delta >= 500) {
        const currentFps = (frameCount * 1000) / delta;
        setFps(currentFps);
        frameCount = 0;
        lastTime = time;

        if (!performanceModeRef.current && currentFps < FPS_LOW_THRESHOLD) {
          performanceModeRef.current = true;
          setPerformanceMode(true);
        } else if (performanceModeRef.current && currentFps > FPS_HIGH_THRESHOLD) {
          performanceModeRef.current = false;
          setPerformanceMode(false);
        }
      }
      rafId = requestAnimationFrame(loop);
    };

    rafId = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(rafId);
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
    <div className="relative h-[70vh] md:h-[80vh] w-full">
      <div className="absolute right-4 top-4 rounded bg-slate-900/70 px-2 py-1 text-xs text-slate-200">
        FPS: {fps.toFixed(0)} {performanceMode ? "(perf)" : ""}
      </div>
      <Globe
        ref={globeRef}
        backgroundColor="rgba(0,0,0,0)"

        /* атмосфера и фон */
        showAtmosphere={!performanceMode}
        atmosphereColor={SEA_GLOW}
        atmosphereAltitude={performanceMode ? 0.2 : 0.25}
        globeImageUrl="//unpkg.com/three-globe/example/img/earth-dark.jpg"
        bumpImageUrl={performanceMode ? undefined : "//unpkg.com/three-globe/example/img/earth-topology.png"}

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
        labelResolution={performanceMode ? 1 : 2}
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
        arcAltitude={performanceMode ? 0.18 : 0.25}
        arcStroke={performanceMode ? 0.45 : 0.6}
        arcDashLength={performanceMode ? 0 : 0.45}
        arcDashGap={performanceMode ? 0 : 0.2}
        arcDashAnimateTime={performanceMode ? 0 : 1600}

        /* интерактив */
        enablePointerInteraction={true}
        onZoom={handleZoom}
      />
    </div>
  );
}

function shallowArcEqual(a: Arc, b: Arc) {
  return (
    a.startLat === b.startLat &&
    a.startLng === b.startLng &&
    a.endLat === b.endLat &&
    a.endLng === b.endLng &&
    a.color === b.color &&
    a.status === b.status
  );
}

function shallowDotEqual(a: Dot, b: Dot) {
  return (
    a.lat === b.lat &&
    a.lng === b.lng &&
    a.color === b.color &&
    a.size === b.size &&
    a.label === b.label
  );
}
