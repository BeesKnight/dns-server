import { useEffect, useMemo, useRef, useState } from "react";
import Globe, { GlobeMethods } from "react-globe.gl";
import type { GlobeProps } from "react-globe.gl";
import { feature } from "topojson-client";
import type { FeatureCollection, Feature, MultiPolygon, Polygon } from "geojson";
import { mapSSE } from "../lib/api";
import type { GeometryCollection, Topology } from "topojson-specification";
import * as THREE from "three";

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

type CountriesTopology = Topology<{
  countries: GeometryCollection;
}>;

/* ---------- цвета ---------- */
const SEA_GLOW = "rgba(56,189,248,0.28)";
const LAND_CAP = "#124f4c";
const LAND_SIDE = "#15524f";
const LAND_STROKE = "rgba(56,189,248,.12)";

const USER_POINT_ID = "user-location";

export default function MapGlobe({ userLocation }: MapGlobeProps) {
  const globeRef = useRef<GlobeMethods | undefined>(undefined);

  const [polygons, setPolygons] = useState<CountryFeature[]>([]);
  const [arcs, setArcs] = useState<Arc[]>([]);
  const [points, setPoints] = useState<Dot[]>([]);
  const globeMaterial = useMemo(() => {
    const material = new THREE.MeshStandardMaterial({
      color: "#0a1e2a",
      emissive: "#06121b",
      emissiveIntensity: 0.35,
      metalness: 0.1,
      roughness: 0.85,
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
          topo,
          topo.objects.countries
        ) as FeatureCollection<Polygon | MultiPolygon, CountryProperties>;

        const feats = fc.features as CountryFeature[];
        setPolygons(feats);

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
        className="relative h-[70vh] md:h-[80vh] w-full"
        style={{
          overflow: "hidden",
          borderRadius: 12,
          background:
            "radial-gradient(circle at 20% 30%, rgba(34,197,247,0.22), transparent 55%), " +
            "radial-gradient(circle at 80% 15%, rgba(56,189,248,0.18), transparent 60%), " +
            "linear-gradient(180deg,#050b1a 0%, #09142a 55%, #03070f 100%)",
        }}
      >
        <Globe
          ref={globeRef}
          backgroundColor="rgba(0,0,0,0)"
          globeMaterial={globeMaterial}

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
  );
}
