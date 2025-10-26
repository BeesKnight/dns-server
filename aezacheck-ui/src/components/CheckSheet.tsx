import { useMemo, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import {
  Globe2,
  ChevronUp,
  ChevronDown,
  Loader2,
  RotateCcw,
  AlertCircle,
  CheckCircle2,
  Clock,
  Target,
  Network,
  MapPin,
} from "lucide-react";
import { useConnection } from "../store/connection";
import { useCheckRun } from "../store/checkRuns";
import type { CheckKindState } from "../store/checkRuns";

type Props = {
  open: boolean;
  onOpenChange: (v: boolean) => void;
};

export default function CheckSheet({ open, onOpenChange }: Props) {
  const [url, setUrl] = useState("");
  const [formError, setFormError] = useState<string | null>(null);

  const { current, starting, startError, streamError, start: startCheck, reset } = useCheckRun(open);

  const { ip, geo, loading: connectionLoading, error: connectionError } = useConnection();

  const ipText = connectionLoading ? "Загрузка..." : ip ?? "—";
  const providerText = connectionLoading
    ? "Загрузка..."
    : geo
      ? geo.asn_org || (geo.asn ? `AS${geo.asn}` : "—")
      : connectionError
        ? "Недоступно"
        : "—";
  const locationText = connectionLoading
    ? "Загрузка..."
    : geo
      ? [geo.city, geo.country].filter(Boolean).join(", ") || "—"
      : connectionError
        ? "Недоступно"
        : "—";

  const kindItems = useMemo<CheckKindState[]>(() => {
    if (!current) return [];
    const order = ["http", "https", "dns", "ping", "trace"];
    const items = Object.values(current.kinds) as CheckKindState[];
    return items.sort((a, b) => {
      const ai = order.indexOf(a.kind);
      const bi = order.indexOf(b.kind);
      if (ai === -1 && bi === -1) return a.kind.localeCompare(b.kind);
      if (ai === -1) return 1;
      if (bi === -1) return -1;
      return ai - bi;
    });
  }, [current]);

  const traceHops = current?.trace ?? null;

  const handleUrlChange = (value: string) => {
    setUrl(value);
    if (formError) {
      setFormError(null);
    }
    const trimmed = value.trim();
    if (current && trimmed && trimmed !== current.url) {
      reset();
    }
    if (!trimmed && (current || startError)) {
      reset();
    }
  };

  async function runQuick() {
    setFormError(null);
    const trimmed = url.trim();
    if (!trimmed) {
      setFormError("Укажи домен или URL");
      return;
    }
    await startCheck(trimmed);
  }

  const statusBadge = (status?: string) => {
    if (!status) {
      return { text: "Нет данных", className: "border-white/10 text-slate-300" };
    }
    const normalized = status.toLowerCase();
    if (normalized === "ok" || normalized === "success") {
      return {
        text: "Успешно",
        className: "border-emerald-500/40 bg-emerald-500/10 text-emerald-300",
      };
    }
    if (normalized === "running" || normalized === "pending" || normalized === "in_progress") {
      return {
        text: "Выполняется",
        className: "border-blue-500/40 bg-blue-500/10 text-blue-300",
      };
    }
    if (normalized === "error" || normalized === "failed" || normalized === "timeout") {
      return {
        text: "Ошибка",
        className: "border-red-500/40 bg-red-500/10 text-red-300",
      };
    }
    if (normalized === "cancelled" || normalized === "canceled") {
      return {
        text: "Отменено",
        className: "border-amber-500/40 bg-amber-500/10 text-amber-300",
      };
    }
    return {
      text: status,
      className: "border-white/10 bg-slate-800/60 text-slate-200",
    };
  };

  const formatGeo = (geo?: { city?: string | null; country?: string | null } | null) => {
    if (!geo) return null;
    const parts = [geo.city, geo.country].filter(Boolean);
    if (parts.length === 0) return null;
    return parts.join(", ");
  };

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
                      onChange={(e) => handleUrlChange(e.target.value)}
                      onKeyDown={(e) => e.key === "Enter" && runQuick()}
                      placeholder="https://example.com"
                      className="mt-2 w-full rounded-xl border border-white/10 bg-slate-800/60
                                 px-4 py-3 text-slate-100 placeholder:text-slate-500
                                 outline-none focus:ring-2 focus:ring-blue-500/60"
                    />

                    {(formError || startError) && (
                      <div className="mt-3 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-300">
                        {formError ?? startError}
                      </div>
                    )}

                    <div className="mt-4">
                      <button
                        onClick={runQuick}
                        disabled={starting}
                        className="w-full rounded-xl bg-blue-600 px-4 py-3
                                   font-medium text-white shadow
                                   hover:bg-blue-500 disabled:opacity-60"
                      >
                        {starting ? "Запускаем..." : "Запустить проверку"}
                      </button>
                    </div>
                  </div>

                  {/* Правая часть — «Ваше подключение» */}
                  <div className="rounded-xl border border-white/10 bg-slate-800/40 p-4">
                    <div className="text-slate-300 text-sm">Ваше подключение</div>
                    <div className="mt-2 text-slate-200">
                      <div className="text-xs uppercase text-slate-400">IP</div>
                      <div className="text-sm">{ipText}</div>
                    </div>
                    <div className="mt-3 text-slate-200">
                      <div className="text-xs uppercase text-slate-400">Провайдер</div>
                      <div className="text-sm opacity-70">{providerText}</div>
                    </div>
                    <div className="mt-3 text-slate-200">
                      <div className="text-xs uppercase text-slate-400">Локация</div>
                      <div className="text-sm opacity-70">{locationText}</div>
                    </div>
                    <div className="mt-4 text-xs text-slate-400">
                      События в реальном времени: <code>check.start</code>, <code>check.result</code>,{" "}
                      <code>check.done</code>.
                    </div>
                  </div>
                </div>

                <div className="border-t border-white/5 bg-slate-900/40 px-6 py-6">
                  <div className="grid gap-6 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
                    <div className="space-y-4">
                      <div className="flex flex-wrap items-center justify-between gap-3">
                        <div>
                          <h4 className="text-base font-semibold text-slate-100">Результаты проверки</h4>
                          {current ? (
                            <div className="space-y-1 text-xs text-slate-400">
                              <p>
                                Активная проверка: <span className="text-slate-200">{current.url}</span>
                              </p>
                              {(() => {
                                const badge = statusBadge(
                                  current.status === "done"
                                    ? current.doneStatus ?? current.status
                                    : current.status
                                );
                                if (!badge) return null;
                                return (
                                  <span
                                    className={`inline-flex items-center gap-2 rounded-full border px-2.5 py-1 text-[11px] font-medium ${badge.className}`}
                                  >
                                    {current.status === "running" && (
                                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                                    )}
                                    {badge.text}
                                  </span>
                                );
                              })()}
                            </div>
                          ) : (
                            <p className="text-xs text-slate-500">
                              Запусти проверку, чтобы увидеть прогресс и трассировку.
                            </p>
                          )}
                        </div>
                        {current && (
                          <button
                            onClick={() => startCheck(current.url)}
                            disabled={starting || current.status === "running"}
                            className="flex items-center gap-2 rounded-lg border border-white/10 px-3 py-1.5 text-sm text-slate-200 transition hover:border-white/20 disabled:cursor-not-allowed disabled:opacity-60"
                          >
                            <RotateCcw className="h-4 w-4" />
                            Повторить
                          </button>
                        )}
                      </div>

                      {streamError && (
                        <div className="flex items-start gap-3 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-200">
                          <AlertCircle className="mt-0.5 h-4 w-4" />
                          <span>{streamError}</span>
                        </div>
                      )}

                      {!current && (
                        <div className="rounded-xl border border-white/10 bg-slate-800/40 px-4 py-6 text-center text-sm text-slate-400">
                          Результатов пока нет. Запусти «Быструю проверку», чтобы получить события.
                        </div>
                      )}

                      {current && kindItems.length === 0 && (
                        <div className="rounded-xl border border-white/10 bg-slate-800/40 px-4 py-6 text-sm text-slate-300">
                          {current.status === "running" ? (
                            <div className="flex items-center gap-2">
                              <Loader2 className="h-4 w-4 animate-spin" />
                              <span>Ожидаем события от агентов…</span>
                            </div>
                          ) : (
                            <div className="flex items-start gap-2">
                              <AlertCircle className="mt-0.5 h-4 w-4 text-amber-300" />
                              <span>Событий не поступило. Попробуй запустить проверку ещё раз.</span>
                            </div>
                          )}
                        </div>
                      )}

                      {current && kindItems.length > 0 && (
                        <ul className="grid gap-4">
                          {kindItems.map((kind) => {
                            const badge = statusBadge(kind.status);
                            const agent = kind.agent ?? current.agent;
                            const geoText = formatGeo(kind.target?.geo);
                            const agentGeo = formatGeo(agent?.geo);
                            const updatedAt = kind.updatedAt ? new Date(kind.updatedAt) : null;
                            const updatedLabel =
                              updatedAt && !Number.isNaN(updatedAt.getTime())
                                ? updatedAt.toLocaleTimeString()
                                : null;
                            return (
                              <li
                                key={`${kind.kind}-${kind.updatedAt ?? ""}`}
                                className="rounded-xl border border-white/10 bg-slate-800/40 p-4"
                              >
                                <div className="flex flex-wrap items-center justify-between gap-3">
                                  <div>
                                    <div className="flex items-center gap-2 text-sm font-semibold text-slate-100">
                                      <Target className="h-4 w-4 text-blue-300" />
                                      <span>{kind.kind.toUpperCase()}</span>
                                    </div>
                                    {updatedLabel && (
                                      <div className="mt-1 flex items-center gap-1 text-xs text-slate-500">
                                        <Clock className="h-3.5 w-3.5" />
                                        <span>{updatedLabel}</span>
                                      </div>
                                    )}
                                  </div>
                                  <div
                                    className={`rounded-full border px-3 py-1 text-xs font-medium ${badge.className}`}
                                  >
                                    {badge.text}
                                  </div>
                                </div>

                                {kind.target && (
                                  <div className="mt-3 space-y-1 text-sm text-slate-200">
                                    <div className="flex items-center gap-2 text-xs uppercase text-slate-500">
                                      <Network className="h-3.5 w-3.5" />
                                      Цель
                                    </div>
                                    <div className="text-sm text-slate-100">
                                      {kind.target.host ?? kind.target.ip ?? "—"}
                                    </div>
                                    {kind.target.ip && kind.target.host && (
                                      <div className="text-xs text-slate-500">{kind.target.ip}</div>
                                    )}
                                    {geoText && (
                                      <div className="flex items-center gap-2 text-xs text-slate-500">
                                        <MapPin className="h-3.5 w-3.5" />
                                        {geoText}
                                      </div>
                                    )}
                                  </div>
                                )}

                                {agent && (
                                  <div className="mt-3 space-y-1 text-sm text-slate-200">
                                    <div className="flex items-center gap-2 text-xs uppercase text-slate-500">
                                      <Globe2 className="h-3.5 w-3.5" />
                                      Агент
                                    </div>
                                    <div className="text-sm text-slate-100">
                                      {agent.name || agent.id || "—"}
                                    </div>
                                    {agent.ip && (
                                      <div className="text-xs text-slate-500">{agent.ip}</div>
                                    )}
                                    {agentGeo && (
                                      <div className="flex items-center gap-2 text-xs text-slate-500">
                                        <MapPin className="h-3.5 w-3.5" />
                                        {agentGeo}
                                      </div>
                                    )}
                                  </div>
                                )}

                                {kind.reason && (
                                  <div className="mt-3 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs text-red-200">
                                    {kind.reason}
                                  </div>
                                )}
                              </li>
                            );
                          })}
                        </ul>
                      )}

                      {current && current.source && (
                        <div className="rounded-xl border border-white/10 bg-slate-800/40 p-4 text-sm text-slate-200">
                          <div className="flex items-center gap-2 text-xs uppercase text-slate-500">
                            <Network className="h-3.5 w-3.5" />
                            Источник запроса
                          </div>
                          <div className="mt-1 text-sm text-slate-100">{current.source.ip ?? "—"}</div>
                          {formatGeo(current.source.geo) && (
                            <div className="flex items-center gap-2 text-xs text-slate-500">
                              <MapPin className="h-3.5 w-3.5" />
                              {formatGeo(current.source.geo)}
                            </div>
                          )}
                        </div>
                      )}
                    </div>

                    <div className="rounded-xl border border-white/10 bg-slate-800/40 p-4">
                      <div className="flex items-center gap-2">
                        <div className="h-9 w-9 rounded-full bg-blue-500/15 text-blue-300 grid place-items-center">
                          <Network className="h-4 w-4" />
                        </div>
                        <div>
                          <h5 className="text-sm font-semibold text-slate-100">Трассировка</h5>
                          <p className="text-xs text-slate-500">Переходы агента до цели</p>
                        </div>
                      </div>

                      {current?.geoLoading && (
                        <div className="mt-4 flex items-center gap-2 text-sm text-slate-300">
                          <Loader2 className="h-4 w-4 animate-spin" />
                          <span>Собираем геоданные…</span>
                        </div>
                      )}

                      {!current && (
                        <div className="mt-4 text-sm text-slate-400">Нет данных по трассировке.</div>
                      )}

                      {current && !current.geoLoading && !traceHops?.length && (
                        <div className="mt-4 rounded-lg border border-white/10 bg-slate-900/40 px-3 py-3 text-sm text-slate-400">
                          События трассировки не поступили.
                        </div>
                      )}

                      {traceHops?.length ? (
                        <ol className="mt-4 space-y-3">
                          {traceHops.map((hop, index) => (
                            <li key={`${hop.n ?? index}-${hop.ip ?? index}`} className="relative flex gap-3">
                              <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-blue-500/40 bg-blue-500/10 text-xs text-blue-200">
                                {hop.n ?? index + 1}
                              </div>
                              <div className="flex-1">
                                <div className="text-sm text-slate-100">{hop.ip ?? "—"}</div>
                                {formatGeo(hop.geo) && (
                                  <div className="flex items-center gap-2 text-xs text-slate-500">
                                    <MapPin className="h-3.5 w-3.5" />
                                    {formatGeo(hop.geo)}
                                  </div>
                                )}
                              </div>
                              {index < traceHops.length - 1 && (
                                <span className="absolute left-3 top-6 h-full w-px bg-blue-500/20" aria-hidden />
                              )}
                            </li>
                          ))}
                        </ol>
                      ) : null}

                      {current && current.geoError && (
                        <div className="mt-4 flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-200">
                          <AlertCircle className="mt-0.5 h-4 w-4" />
                          <span>{current.geoError}</span>
                        </div>
                      )}

                      {current && current.status === "done" && !current.geoLoading && traceHops?.length ? (
                        <div className="mt-4 flex items-center gap-2 text-sm text-emerald-300">
                          <CheckCircle2 className="h-4 w-4" />
                          <span>Трассировка завершена</span>
                        </div>
                      ) : null}
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
