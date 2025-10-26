import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import MapGlobe from "../components/MapGlobe";
import Map2D from "../components/Map2D";
import CheckSheet from "../components/CheckSheet";
import { useConnection } from "../store/connection";

function BgFX() {
  return (
    <div className="pointer-events-none absolute inset-0 -z-10">
      <div className="absolute inset-0 bg-[#0b1220]" />
      <div className="absolute inset-0 bg-[radial-gradient(85vmin_85vmin_at_32%_58%,rgba(63,130,255,0.45)_0%,rgba(63,130,255,0.22)_40%,transparent_70%)]" />
      <div className="absolute inset-0 bg-[radial-gradient(65vmin_65vmin_at_85%_18%,rgba(63,130,255,0.26)_0%,transparent_70%)]" />
      <div className="absolute inset-0 bg-[radial-gradient(120vmax_80vmax_at_20%_110%,rgba(28,46,96,0.45)_0%,transparent_60%)]" />
      <div className="absolute inset-0 bg-[radial-gradient(120vmax_80vmax_at_50%_50%,transparent_65%,rgba(0,0,0,0.50)_100%)]" />
    </div>
  );
}

export default function Home() {
  const [mode, setMode] = useState<"2D" | "3D">("3D");
  const [open, setOpen] = useState(true);
  const { geo, ip } = useConnection();

  const userLocation = useMemo(() => {
    if (!geo) return null;
    const { lat, lon, city, country } = geo;
    if (typeof lat !== "number" || typeof lon !== "number") {
      return null;
    }

    const labelParts: string[] = [];
    if (city) labelParts.push(city);
    if (country) labelParts.push(country);
    const locationText = labelParts.join(", ").trim();
    const label = locationText.length > 0 ? locationText : (ip ? `IP ${ip}` : undefined);

    return { lat, lon, label };
  }, [geo, ip]);

  return (
    <div className="relative min-h-screen overflow-hidden">
      <BgFX />

      {/* переключатель слева */}
      <div className="absolute left-4 top-4 z-50">
        <div className="flex rounded-xl bg-black/30 backdrop-blur-md ring-1 ring-white/10 overflow-hidden">
          <button
            onClick={() => setMode("2D")}
            className={`px-3 py-2 text-sm font-semibold ${mode === "2D" ? "bg-white/15 text-white" : "text-slate-300 hover:bg-white/10"}`}
            aria-pressed={mode === "2D"}
          >
            2D
          </button>
          <button
            onClick={() => setMode("3D")}
            className={`px-3 py-2 text-sm font-semibold ${mode === "3D" ? "bg-white/15 text-white" : "text-slate-300 hover:bg-white/10"}`}
            aria-pressed={mode === "3D"}
          >
            3D
          </button>
        </div>
      </div>

      {/* Кнопка справа сверху (стеклянный стиль проекта) */}
      <div className="absolute right-4 top-4 z-50 flex items-center gap-2">
        <Link
          to="/app/agents"
          className="inline-flex items-center gap-2 rounded-xl border border-white/10
                     bg-black/30 hover:bg-white/10 backdrop-blur-md
                     px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg transition"
          aria-label="Открыть консоль агентов"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path d="M4 5h16M4 10h16M4 15h10" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
            <circle cx="18" cy="15" r="1.5" stroke="currentColor" strokeWidth="1.6" />
          </svg>
          Агенты
        </Link>
        <Link
          to="/app/services"
          className="inline-flex items-center gap-2 rounded-xl border border-white/10
                     bg-black/30 hover:bg-white/10 backdrop-blur-md
                     px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg transition"
          aria-label="Открыть популярные сервисы"
        >
          {/* иконка-сетка */}
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <rect x="3"  y="3"  width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6"/>
            <rect x="14" y="3"  width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6"/>
            <rect x="3"  y="14" width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6"/>
            <rect x="14" y="14" width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6"/>
          </svg>
          Сервисы
        </Link>
      </div>

      {/* карта */}
      <div className="px-4 py-8 md:py-10">
        {mode === "2D" ? <Map2D userLocation={userLocation} /> : <MapGlobe userLocation={userLocation} />}
      </div>

      <CheckSheet open={open} onOpenChange={setOpen} />
    </div>
  );
}
