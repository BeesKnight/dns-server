import { useEffect, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Globe2, ChevronUp, ChevronDown } from "lucide-react";
import { api } from "../lib/api";

type Props = {
  open: boolean;
  onOpenChange: (v: boolean) => void;
};

export default function CheckSheet({ open, onOpenChange }: Props) {
  const [url, setUrl] = useState("");
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // лёгкая инфа о текущем подключении (берём из /v1/auth/me)
  const [ip, setIp] = useState<string | null>(null);
  useEffect(() => {
    api
      .me()
      .then((u) => setIp(u.current_ip || null))
      .catch(() => {});
  }, []);

  async function runQuick() {
    setErr(null);
    if (!url.trim()) {
      setErr("Укажи домен или URL");
      return;
    }
    try {
      setLoading(true);
      await api.startQuickCheck(url.trim());
      // тут можно показать тост/уведомление «Проверка запущена»
    } catch (error: unknown) {
      const message =
        error instanceof Error ? error.message : "Не удалось запустить проверку";
      setErr(message);
    } finally {
      setLoading(false);
    }
  }

  return (
    <>
      {/* Язычок */}
      <button
        onClick={() => onOpenChange(!open)}
        className="fixed left-1/2 -translate-x-1/2 bottom-3 z-40
                   rounded-full border border-white/10 bg-slate-900/70
                   backdrop-blur-md px-4 py-2 text-slate-200 shadow-lg
                   hover:bg-slate-900/80 transition"
        aria-label={open ? "Скрыть панель" : "Показать панель"}
      >
        <div className="flex items-center gap-2">
          {open ? <ChevronDown size={16} /> : <ChevronUp size={16} />}
          <span className="text-sm">Панель проверки</span>
        </div>
      </button>

      {/* Сам шит */}
      <AnimatePresence>
        {open && (
          <motion.div
            key="sheet"
            initial={{ y: 32, opacity: 0 }}
            animate={{ y: 0, opacity: 1 }}
            exit={{ y: 24, opacity: 0 }}
            transition={{ type: "spring", stiffness: 260, damping: 24 }}
            className="fixed inset-x-0 bottom-0 z-30"
          >
            {/* Полоса блюра за шитом, чтобы фон мягче читался */}
            <div className="pointer-events-none absolute inset-x-0 bottom-0 h-[54vh] bg-gradient-to-t from-slate-950/70 to-transparent" />

            <div className="mx-auto max-w-5xl px-4 pb-8">
              <div
                className="rounded-2xl border border-white/10 bg-slate-900/55
                           backdrop-blur-xl shadow-2xl"
              >
                {/* Заголовок */}
                <div className="flex items-center gap-3 px-6 pt-5">
                  <div className="grid h-9 w-9 place-items-center rounded-full bg-blue-500/15 text-blue-300">
                    <Globe2 size={18} />
                  </div>
                  <div>
                    <h3 className="text-slate-100 text-lg font-semibold">
                      Проверить сайт
                    </h3>
                    <p className="text-slate-400 text-xs">
                      Шаблон quick: HTTPS GET + ping + DNS.
                    </p>
                  </div>
                </div>

                {/* Контент */}
                <div className="grid gap-4 px-6 py-5 md:grid-cols-3">
                  {/* Левая часть — форма */}
                  <div className="md:col-span-2">
                    <label className="text-sm text-slate-300">Домен или URL</label>
                    <input
                      value={url}
                      onChange={(e) => setUrl(e.target.value)}
                      onKeyDown={(e) => e.key === "Enter" && runQuick()}
                      placeholder="https://example.com"
                      className="mt-2 w-full rounded-xl border border-white/10 bg-slate-800/60
                                 px-4 py-3 text-slate-100 placeholder:text-slate-500
                                 outline-none focus:ring-2 focus:ring-blue-500/60"
                    />

                    {err && (
                      <div className="mt-3 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-300">
                        {err}
                      </div>
                    )}

                    <div className="mt-4">
                      <button
                        onClick={runQuick}
                        disabled={loading}
                        className="w-full rounded-xl bg-blue-600 px-4 py-3
                                   font-medium text-white shadow
                                   hover:bg-blue-500 disabled:opacity-60"
                      >
                        {loading ? "Запускаем..." : "Запустить проверку"}
                      </button>
                    </div>
                  </div>

                  {/* Правая часть — «Ваше подключение» */}
                  <div className="rounded-xl border border-white/10 bg-slate-800/40 p-4">
                    <div className="text-slate-300 text-sm">Ваше подключение</div>
                    <div className="mt-2 text-slate-200">
                      <div className="text-xs uppercase text-slate-400">IP</div>
                      <div className="text-sm">{ip ?? "—"}</div>
                    </div>
                    {/* Места под провайдера/местоположение; заполни, когда появится эндпоинт */}
                    <div className="mt-3 text-slate-200">
                      <div className="text-xs uppercase text-slate-400">Провайдер</div>
                      <div className="text-sm opacity-70">—</div>
                    </div>
                    <div className="mt-3 text-slate-200">
                      <div className="text-xs uppercase text-slate-400">Локация</div>
                      <div className="text-sm opacity-70">—</div>
                    </div>
                    <div className="mt-4 text-xs text-slate-400">
                      События в реальном времени: <code>check.start</code>,{" "}
                      <code>check.done</code>, <code>agents</code>.
                    </div>
                  </div>
                </div>
              </div>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </>
  );
}
