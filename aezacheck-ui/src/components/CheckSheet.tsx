import { useEffect, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { api } from "../lib/api";
import useAuth from "../store/auth";
import { Globe2, ArrowUpRight } from "lucide-react";

type IpInfo = {
  ip?: string;
  asn?: string;
  org?: string;
  city?: string;
  country?: string;
};

export default function CheckSheet() {
  const { user } = useAuth();
  const [open, setOpen] = useState(false);

  // форма
  const [url, setUrl] = useState("");
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // инфо о пользователе (ip/org/geo)
  const [ipInfo, setIpInfo] = useState<IpInfo>({});

  // подтянем текущий IP из /v1/auth/me; затем попробуем гео (если бек реализован)
  useEffect(() => {
    let stopped = false;

    async function load() {
      try {
        const me = await api.me().catch(() => null);
        if (!me || stopped) return;

        const ip = (me as any).current_ip as string | undefined;
        const next: IpInfo = { ip };

        // необязательный запрос — если эндпоинт еще не реализован, молча игнорим
        if (ip) {
          try {
            const g = await (api as any).geoIp?.(ip); // ожидаемый ответ: { city,country,asn,org }
            if (g) {
              next.city = g.city;
              next.country = g.country;
              next.asn = g.asn || g.ASN;
              next.org = g.org || g.isp;
            }
          } catch {
            /* ignore, не критично */
          }
        }

        if (!stopped) setIpInfo(next);
      } catch {
        /* ignore */
      }
    }

    load();
    return () => {
      stopped = true;
    };
  }, []);

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    if (!url.trim()) {
      setErr("Укажите домен или URL.");
      return;
    }
    try {
      setLoading(true);
      // минимальный «quick»: http + ping + dns
      await api.startQuickCheck(url.trim());
      // тут можно показать тост «проверка запущена»
    } catch (e: any) {
      setErr(e?.message || "Не удалось запустить проверку");
    } finally {
      setLoading(false);
    }
  }

  return (
    <>
      {/* размытие фона при открытии */}
      <AnimatePresence>
        {open && (
          <motion.div
            key="backdrop"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.2 }}
            className="fixed inset-0 z-30 bg-black/20 backdrop-blur-sm"
            onClick={() => setOpen(false)}
          />
        )}
      </AnimatePresence>

      {/* сам bottom-sheet */}
      <div className="fixed inset-x-0 bottom-0 z-40 pointer-events-none">
        <motion.div
          initial={false}
          animate={{ y: open ? 0 : 260 }} // высота в закрытом состоянии (виден «язычок»)
          transition={{ type: "spring", stiffness: 300, damping: 30 }}
          className="mx-auto w-full max-w-4xl pointer-events-auto"
        >
          <div className="mx-3 md:mx-6 rounded-t-3xl border border-white/10 bg-slate-900/70 backdrop-blur-xl shadow-2xl">
            {/* язычок */}
            <div className="flex justify-center pt-3">
              <button
                onClick={() => setOpen((v) => !v)}
                className="h-1.5 w-12 rounded-full bg-slate-200/70 hover:bg-slate-100 transition"
                aria-label={open ? "Скрыть панель" : "Показать панель"}
              />
            </div>

            {/* заголовок + короткий статус */}
            <div className="px-4 md:px-6 pb-2 pt-3 flex items-center gap-3">
              <div className="p-1.5 rounded-lg bg-blue-500/15 text-blue-300">
                <Globe2 className="w-5 h-5" />
              </div>
              <div className="flex-1">
                <h3 className="text-slate-100 text-lg font-semibold">Проверить сайт</h3>
                <p className="text-slate-400 text-sm">Шаблон quick: HTTPS GET + ping + DNS</p>
              </div>
              <button
                onClick={() => setOpen((v) => !v)}
                className="hidden md:inline-flex text-slate-300 hover:text-white transition"
              >
                {open ? "Скрыть" : "Показать"}
              </button>
            </div>

            {/* контент */}
            <div className="px-4 md:px-6 pb-5">
              {/* блок с инфо о пользователе */}
              <div className="grid grid-cols-1 sm:grid-cols-3 gap-3 mb-4">
                <InfoItem label="Ваш IP" value={ipInfo.ip || "—"} />
                <InfoItem
                  label="Провайдер / ASN"
                  value={
                    ipInfo.org || ipInfo.asn
                      ? [ipInfo.org, ipInfo.asn].filter(Boolean).join(" · ")
                      : "—"
                  }
                />
                <InfoItem
                  label="Геолокация"
                  value={
                    ipInfo.city || ipInfo.country
                      ? [ipInfo.city, ipInfo.country].filter(Boolean).join(", ")
                      : "—"
                  }
                />
              </div>

              {/* форма «Проверить сайт» */}
              <form onSubmit={onSubmit} className="flex flex-col sm:flex-row gap-3">
                <input
                  type="text"
                  inputMode="url"
                  placeholder="https://example.com"
                  className="flex-1 rounded-xl bg-slate-800/70 border border-white/10 px-4 py-3 text-slate-100 placeholder:text-slate-500 focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                  value={url}
                  onChange={(e) => setUrl(e.target.value)}
                />
                <button
                  disabled={loading}
                  className="inline-flex items-center justify-center gap-2 rounded-xl bg-blue-600 hover:bg-blue-500 disabled:bg-blue-600/60 text-white px-5 py-3 font-medium transition"
                >
                  Запустить проверку
                  <ArrowUpRight className="w-4 h-4" />
                </button>
              </form>

              {err && (
                <div className="mt-3 rounded-xl bg-red-500/15 border border-red-500/30 text-red-200 px-3 py-2 text-sm">
                  {err}
                </div>
              )}

              {/* «подсказки» внизу */}
              <div className="mt-4 text-xs text-slate-400">
                Панель можно скрыть — кликните по язычку. Во время проверки события появятся на
                карте (дуги agent → target) и в реальном времени через SSE.
              </div>
            </div>
          </div>
        </motion.div>
      </div>
    </>
  );
}

function InfoItem({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-xl border border-white/10 bg-slate-800/50 px-3 py-2">
      <div className="text-[11px] uppercase tracking-wide text-slate-400">{label}</div>
      <div className="text-sm text-slate-100 mt-0.5">{value}</div>
    </div>
  );
}
