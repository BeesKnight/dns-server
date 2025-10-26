import { useEffect, useMemo, useRef, useState } from "react";
import Globe, { GlobeMethods } from "react-globe.gl";
import type { GlobeProps } from "react-globe.gl";
import { feature } from "topojson-client";
import { geoCentroid, geoArea } from "d3-geo";
import type { FeatureCollection, Feature, MultiPolygon, Polygon } from "geojson";
import { mapSSE } from "../lib/api";
import type { GeometryCollection, Topology } from "topojson-specification";

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

/* ---------- типы стран/подписей ---------- */
type CountryProperties = { name?: string; ADMIN?: string; name_long?: string };

type CountryFeature = Feature<Polygon | MultiPolygon, CountryProperties>;

type CountriesTopology = Topology<{
  countries: GeometryCollection;
}>;

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

const USER_POINT_ID = "user-location";

export default function MapGlobe({ userLocation }: MapGlobeProps) {
  const globeRef = useRef<GlobeMethods | undefined>(undefined);
  const camAltRef = useRef(1.6);         // текущая высота камеры
  const [tick, setTick] = useState(0);   // лёгкий триггер пересчёта LOD

  const [polygons, setPolygons] = useState<CountryFeature[]>([]);
  const [labelsAll, setLabelsAll] = useState<CountryLabel[]>([]);
  const [visibleLabels, setVisibleLabels] = useState<CountryLabel[]>([]);
  const [arcs, setArcs] = useState<Arc[]>([]);
  const [points, setPoints] = useState<Dot[]>([]);

  /* ---------- загрузка карт и расчёт подписей ---------- */
  useEffect(() => {
    // стартовая точка обзора
    globeRef.current?.pointOfView({ lat: 30, lng: 20, altitude: 1.6 }, 1200);

    fetch("https://unpkg.com/world-atlas@2.0.2/countries-110m.json")
      .then((r) => r.json())
      .then((topologyRaw: unknown) => {
        const topo = topologyRaw as CountriesTopology;
        const fc = feature(
          topo,
          topo.objects.countries
        ) as FeatureCollection<Polygon | MultiPolygon, CountryProperties>;

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
            const featureGeometry = f as Feature<Polygon | MultiPolygon, CountryProperties>;
            const c = geoCentroid(featureGeometry);
            const area = geoArea(featureGeometry);

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

  useEffect(() => {
    const alt = camAltRef.current;

    let take = 0;
    if (alt > 2.2) take = 20;
    else if (alt > 1.7) take = 45;
    else if (alt > 1.4) take = 80;
    else if (alt > 1.2) take = 120;
    else take = 160;

    setVisibleLabels(labelsAll.slice(0, take));
  }, [labelsAll, tick]);

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
  const computeLabelSize = (d: CountryLabel) => {
    const a = d.area; // 0..~0.2
    if (a > 0.08) return 1.35; // крупные (Россия, Канада, Китай)
    if (a > 0.05) return 1.05;
    if (a > 0.02) return 0.85;
    if (a > 0.01) return 0.72;
    if (a > 0.005) return 0.64;
    return 0.56;
  };

  const labelLatAccessor: GlobeProps["labelLat"] = (d) => (d as CountryLabel).lat;
  const labelLngAccessor: GlobeProps["labelLng"] = (d) => (d as CountryLabel).lng;
  const labelTextAccessor: GlobeProps["labelText"] = (d) => (d as CountryLabel).name;
  const labelSizeAccessor: GlobeProps["labelSize"] = (d) =>
    computeLabelSize(d as CountryLabel);
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
        labelLat={labelLatAccessor}
        labelLng={labelLngAccessor}
        labelText={labelTextAccessor}
        labelSize={labelSizeAccessor}
        labelColor={() => LABEL_COLOR}
        labelAltitude={0.03}
        labelResolution={2}
        labelIncludeDot={false}

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
        onZoom={handleZoom}
      />
    </div>
  );
}