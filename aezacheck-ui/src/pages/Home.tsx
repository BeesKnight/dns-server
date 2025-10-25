import MapGlobe from "../components/MapGlobe";
import QuickCheckCard from "../components/QuickCheckCard";
import { ShieldCheck } from "lucide-react";

export default function Home() {
  return (
    <div className="relative min-h-screen overflow-hidden">
      <BgGradients />

      {/* Основная сетка: больше места слева под глобус, справа — панель */}
      <div className="relative z-40 grid grid-cols-1 md:grid-cols-[1.3fr_0.9fr] gap-8 items-center px-6 md:px-10 lg:px-16 py-10">
        {/* Левая колонка: только глобус с локальным сдвигом влево */}
        <div className="order-2 md:order-1 min-w-0">
          <div className="md:-ml-24 lg:-ml-40 xl:-ml-56 2xl:-ml-[22rem]">
            <MapGlobe />
          </div>
        </div>

        {/* Правая колонка: карточка проверки, фиксированный комфортный максимум */}
        <div className="order-1 md:order-2 justify-self-end w-full">
          <QuickCheckCard />
        </div>
      </div>

      {/* Подпись внизу слева — вне «-ml», всегда внутри экрана */}
      <div className="pointer-events-none absolute left-6 md:left-10 bottom-6 flex items-center gap-2 text-slate-300/80">
        <ShieldCheck className="size-5 text-emerald-400" />
        <span className="text-sm">Real-time события: check.start / check.done / agents.</span>
      </div>
    </div>
  );
}

function BgGradients() {
  return (
    <>
      <div className="pointer-events-none absolute inset-0 bg-[radial-gradient(60%_50%_at_60%_30%,rgba(37,99,235,0.35),transparent_60%),radial-gradient(60%_50%_at_0%_100%,rgba(99,102,241,0.35),transparent_60%)]" />
      <div className="pointer-events-none absolute -top-32 -right-32 h-72 w-72 rounded-full bg-white/10 blur-3xl" />
      <div className="pointer-events-none absolute -bottom-16 -left-16 h-64 w-64 rounded-full bg-white/10 blur-3xl" />
    </>
  );
}
