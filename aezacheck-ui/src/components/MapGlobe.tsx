import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Globe, { GlobeMethods } from "react-globe.gl";
import { feature } from "topojson-client";
import { geoCentroid, geoArea, geoDistance } from "d3-geo";
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
const MIN_CAMERA_ALT = 0.55;
const MAX_CAMERA_ALT = 2.6;
const VIEW_EPS = 1e-3;

export default function MapGlobe() {
  const globeRef = useRef<GlobeMethods | null>(null);
  const camAltRef = useRef(1.6); // текущая высота камеры
  const camViewRef = useRef({ lat: 30, lng: 20 });
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
  const polygonCentroidsRef = useRef<[number, number][]>([]);

  const clampAltitude = (value: number) =>
    Math.min(MAX_CAMERA_ALT, Math.max(MIN_CAMERA_ALT, value));

  const queueTick = useCallback(() => {
    if (zoomRaf.current) return;
    zoomRaf.current = requestAnimationFrame(() => {
      zoomRaf.current = null;
      setTick((t) => t + 1);
    });
  }, [setTick]);

  const adjustCameraControls = useCallback((altitude: number) => {
    const controls = globeRef.current?.controls();
    if (!controls) return;

    const extraTilt = Math.max(0, 1.15 - altitude);
    const maxPolar = Math.max(Math.PI / 3, Math.PI / 2 - 0.1 - extraTilt * 0.22);
    const minPolar = Math.max(Math.PI / 6, maxPolar - 0.35);

    controls.maxPolarAngle = maxPolar;
    controls.minPolarAngle = Math.min(minPolar, maxPolar);
  }, []);

  const syncCameraState = useCallback(() => {
    const pov = globeRef.current?.pointOfView();
    if (!pov) return;

    const prevAlt = camAltRef.current;
    const prevView = camViewRef.current;

    const rawAlt = typeof pov.altitude === "number" ? pov.altitude : prevAlt;
    const nextAlt = clampAltitude(rawAlt);

    const lat = typeof pov.lat === "number" ? pov.lat : prevView.lat;
    const lng = typeof pov.lng === "number" ? pov.lng : prevView.lng;

    const latChanged = Math.abs(prevView.lat - lat) > VIEW_EPS;
    const lngChanged = Math.abs(prevView.lng - lng) > VIEW_EPS;
    const altChanged = Math.abs(prevAlt - nextAlt) > VIEW_EPS;

    if (latChanged || lngChanged) {
      camViewRef.current = { lat, lng };
    }

    if (altChanged) {
      camAltRef.current = nextAlt;
    }

    if (Math.abs(nextAlt - rawAlt) > VIEW_EPS) {
      globeRef.current?.pointOfView({ lat, lng, altitude: nextAlt }, 120);
    }

    if (altChanged || latChanged || lngChanged) {
      queueTick();
    }

    adjustCameraControls(nextAlt);
  }, [adjustCameraControls, queueTick]);

  /* ---------- загрузка карт и расчёт подписей ---------- */
  useEffect(() => {
    // стартовая точка обзора
    globeRef.current?.pointOfView({ lat: 30, lng: 20, altitude: 1.6 }, 1200);
    camAltRef.current = 1.6;
    camViewRef.current = { lat: 30, lng: 20 };
    queueTick();
    adjustCameraControls(1.6);

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
        polygonCentroidsRef.current = feats.map((f) => geoCentroid(f as any));

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
  }, [adjustCameraControls, queueTick]);

  useEffect(() => {
    const controls = globeRef.current?.controls();
    if (!controls) return;

    const handleChange = () => syncCameraState();
    controls.addEventListener("change", handleChange);
    syncCameraState();

    return () => {
      controls.removeEventListener("change", handleChange);
    };
  }, [syncCameraState]);

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

  const labelResolution = useMemo(() => {
    const alt = camAltRef.current;
    if (performanceMode) {
      if (alt < 0.68) return 0.18;
      if (alt < 0.9) return 0.32;
      if (alt < 1.2) return 0.48;
      return 0.68;
    }
    if (alt < 0.6) return 0.14;
    if (alt < 0.72) return 0.22;
    if (alt < 0.85) return 0.32;
    if (alt < 1.05) return 0.48;
    if (alt < 1.35) return 0.64;
    return 0.82;
  }, [performanceMode, tick]);

  const labelAltitude = useMemo(() => {
    const alt = camAltRef.current;
    if (alt < 0.65) return 0.006;
    if (alt < 0.85) return 0.01;
    if (alt < 1.1) return 0.015;
    if (alt < 1.45) return 0.02;
    return 0.026;
  }, [tick]);

  const visiblePolygons = useMemo(() => {
    if (!polygons.length) return polygons;
    const alt = camAltRef.current;
    if (alt >= 1.25) return polygons;

    const centroids = polygonCentroidsRef.current;
    const view = camViewRef.current;

    const threshold =
      alt < 0.62 ? 0.78 : alt < 0.75 ? 0.95 : alt < 0.9 ? 1.18 : alt < 1.05 ? 1.45 : 1.95;

    return polygons.filter((_, idx) => {
      const centroid = centroids[idx];
      if (!centroid) return true;
      const dist = geoDistance([view.lng, view.lat], centroid);
      return dist <= threshold;
    });
  }, [polygons, tick]);

  // без setState на каждый zoom — только лёгкий "ping"
  const handleZoom = useCallback(() => {
    syncCameraState();
  }, [syncCameraState]);

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
        polygonsData={visiblePolygons}
        polygonAltitude={0.02}
        polygonCapColor={() => LAND_CAP}
        polygonSideColor={() => LAND_SIDE}
        polygonStrokeColor={() => LAND_STROKE}
        polygonsTransitionDuration={0}

        /* подписи стран (точно по центроиду, чуть приподняты) */
        labelsData={visibleLabels}
        labelLat={(d: CountryLabel) => d.lat}
        labelLng={(d: CountryLabel) => d.lng}
        labelText={(d: CountryLabel) => d.name.toUpperCase()}
        labelSize={labelSize}
        labelColor={() => LABEL_COLOR}
        labelAltitude={labelAltitude}
        labelResolution={labelResolution}
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
