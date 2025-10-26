import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";

import { api, type ServiceReview, type ServiceSummary } from "../lib/api";

/* ====================== Типы и демо-данные ====================== */

function genHourly(n: number, min: number, max: number) {
  const a: number[] = [];
  for (let i = 0; i < n; i++) a.push(Math.floor(min + Math.random() * (max - min)));
  return a;
}

const fallbackServices: ServiceSummary[] = [
  { id: "yt", name: "YouTube",  url: "https://youtube.com",  status: "ok",   rating: 4.7, reviews: 2381, hourlyChecks: genHourly(24, 1200, 2800) },
  { id: "gg", name: "Google",   url: "https://google.com",   status: "ok",   rating: 4.7, reviews: 2210, hourlyChecks: genHourly(24, 1000, 2500) },
  { id: "tg", name: "Telegram", url: "https://telegram.org", status: "ok",   rating: 4.6, reviews: 1975, hourlyChecks: genHourly(24,  900, 2200) },
  { id: "dc", name: "Discord",  url: "https://discord.com",  status: "warn", rating: 4.2, reviews: 1120, hourlyChecks: genHourly(24,  600, 1500) },
  { id: "gh", name: "GitHub",   url: "https://github.com",   status: "ok",   rating: 4.9, reviews:  980, hourlyChecks: genHourly(24,  400,  900) },
];

/* ====================== Утилиты ====================== */
const dot = (s: ServiceSummary["status"]) =>
  s === "ok"
    ? "bg-emerald-400 shadow-[0_0_12px_rgba(16,185,129,.55)]"
    : s === "warn"
    ? "bg-amber-300  shadow-[0_0_12px_rgba(251,191,36,.55)]"
    : "bg-rose-400   shadow-[0_0_12px_rgba(244,63,94,.55)]";

const compact = (n: number) => Intl.NumberFormat("ru-RU", { notation: "compact" }).format(n);
const starsText = (r: number) => Array.from({ length: 5 }, (_, i) => (i < Math.round(r) ? "★" : "☆"));
const round1 = (n: number) => Number(n.toFixed(1));

/* ====================== Главная страница сервисов ====================== */
export default function Services() {
  const [list, setList] = useState<ServiceSummary[]>(fallbackServices);
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<Error | null>(null);
  const [q, setQ] = useState("");
  const [modal, setModal] = useState<{ open: boolean; service?: ServiceSummary }>({ open: false });

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);

    void api
      .listServices()
      .then((items) => {
        if (!cancelled) {
          setList(items);
          setModal((prev) => {
            if (!prev.open || !prev.service) return prev;
            const updated = items.find((it) => it.id === prev.service.id);
            return updated ? { open: true, service: updated } : prev;
          });
        }
      })
      .catch((err) => {
        if (cancelled) return;
        const e = err instanceof Error ? err : new Error("Не удалось загрузить данные о сервисах");
        setError(e);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, []);

  const applyStats = useCallback(
    (serviceId: string, stats: { averageRating: number; reviewCount: number }) => {
      const nextRating = round1(stats.averageRating);
      setList((prev) =>
        prev.map((item) =>
          item.id === serviceId
            ? { ...item, rating: nextRating, reviews: stats.reviewCount }
            : item
        )
      );
      setModal((prev) => {
        if (!prev.open || !prev.service || prev.service.id !== serviceId) return prev;
        return {
          open: true,
          service: { ...prev.service, rating: nextRating, reviews: stats.reviewCount },
        };
      });
    },
    []
  );

  const filtered = useMemo(() => {
    const s = q.trim().toLowerCase();
    return !s ? list : list.filter((x) => (x.name + x.url).toLowerCase().includes(s));
  }, [list, q]);

  const totals = useMemo(
    () =>
      filtered
        .map((s) => ({ id: s.id, name: s.name, total: s.hourlyChecks.reduce((a, b) => a + b, 0) }))
        .sort((a, b) => b.total - a.total),
    [filtered]
  );
  const maxTotal = Math.max(...totals.map((t) => t.total), 1);

  return (
    <div className="relative min-h-screen px-4 pb-8">
      {/* Верхняя панель */}
      <div className="sticky top-4 z-40 mt-4 flex justify-between">
        <Link
          to="/app"
          className="inline-flex items-center gap-2 rounded-2xl border border-white/10
                     bg-slate-900/55 hover:bg-slate-900/70 backdrop-blur-xl
                     px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg transition"
        >
          {/* ← иконка */}
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path d="M15 19l-7-7 7-7" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"/>
          </svg>
          На главную
        </Link>

        <div className="flex items-center gap-2 w-[min(420px,100%)]">
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Найти сервис…"
            className="w-full rounded-2xl border border-white/10 bg-slate-900/55
                       backdrop-blur-xl px-4 py-2 text-slate-200 placeholder:text-slate-400
                       outline-none focus:ring-2 focus:ring-blue-500/60"
          />
        </div>
      </div>

      {loading && (
        <div className="mt-4 text-sm text-slate-400">
          Загружаем актуальные данные о сервисах…
        </div>
      )}
      {error && (
        <div className="mt-3 rounded-2xl border border-rose-500/40 bg-rose-500/10 px-4 py-2 text-sm text-rose-200">
          {error.message}
        </div>
      )}

      <h1 className="mt-6 mb-3 text-2xl font-bold">Популярные сервисы</h1>

      {/* Сравнение по сумме за 24 часа */}
      <div className="rounded-2xl border border-white/10 bg-slate-900/55 backdrop-blur-xl p-4 mb-4">
        <div className="mb-3 text-sm text-slate-300">Количество проверок (24 часа)</div>
        <div className="grid gap-2">
          {totals.map((t) => (
            <div key={t.id} className="flex items-center gap-3">
              <div className="w-36 truncate">{t.name}</div>
              <div className="h-2 flex-1 rounded-full border border-white/10 bg-white/5 overflow-hidden">
                <div
                  className="h-full bg-gradient-to-r from-sky-400 to-cyan-300"
                  style={{ width: `${(t.total / maxTotal) * 100}%` }}
                />
              </div>
              <div className="w-20 text-right text-slate-400">{compact(t.total)}</div>
            </div>
          ))}
        </div>
      </div>

      {/* Сетка плиток */}
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {filtered.map((s) => (
          <div
            key={s.id}
            className="rounded-2xl border border-white/10 bg-slate-900/55 backdrop-blur-xl p-4"
          >
            <div className="flex items-center gap-3">
              <div className="grid size-9 place-items-center rounded-xl bg-white/10 border border-white/10 font-bold">
                {s.name[0]}
              </div>
              <div className="min-w-0">
                <div className="font-semibold leading-tight">{s.name}</div>
                <div className="text-xs text-slate-400 truncate">
                  {s.url.replace(/^https?:\/\//, "")}
                </div>
              </div>
            </div>

            {/* Мини-график со шкалой */}
            <MiniBarsWithAxis data={s.hourlyChecks} />

            <div className="mt-2 flex items-center justify-between text-sm">
              <div className="flex items-center">
                <span className={`mr-2 inline-block size-2.5 rounded-full ${dot(s.status)}`} />
                {s.status === "ok" ? "Работает" : s.status === "warn" ? "С перебоями" : "Недоступен"}
              </div>
              <div className="text-slate-300">
                {starsText(s.rating).map((ch, i) => (
                  <span key={i} className={ch === "☆" ? "opacity-40" : ""}>
                    {ch}
                  </span>
                ))}
                <span className="ml-1 text-slate-400">{s.rating.toFixed(1)}</span>
              </div>
            </div>

            <div className="mt-1 flex items-center justify-between text-sm">
              <div className="text-slate-400">Проверок сегодня</div>
              <div className="font-semibold">
                {compact(s.hourlyChecks.reduce((a, b) => a + b, 0))}
              </div>
            </div>

            <div className="mt-1 flex items-center justify-between text-sm">
              <div className="text-slate-400">Отзывы</div>
              <div className="font-semibold">{compact(s.reviews)}</div>
            </div>

            <div className="mt-3 flex justify-end gap-2">
              <button
                onClick={() => setModal({ open: true, service: s })}
                className="inline-flex items-center gap-2 rounded-2xl border border-white/10
                           bg-slate-900/55 hover:bg-slate-900/70 backdrop-blur-xl
                           px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg transition"
              >
                {/* иконка чата */}
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                  <path d="M21 12a8 8 0 1 1-3-6.3" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
                  <path d="M22 3l-3.5 3.5" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
                </svg>
                Отзывы
              </button>
              <a
                href={s.url}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-2 rounded-2xl border border-white/10
                           bg-slate-900/55 hover:bg-slate-900/70 backdrop-blur-xl
                           px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg transition"
              >
                Открыть
                {/* ↗ */}
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                  <path d="M7 17L17 7M17 7H8M17 7V16" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
                </svg>
              </a>
            </div>
          </div>
        ))}
      </div>

      {/* Модалка отзывов */}
      {modal.open && modal.service && (
        <ReviewsModal
          service={modal.service}
          onClose={() => setModal({ open: false })}
          onStats={applyStats}
        />
      )}
    </div>
  );
}

/* ====================== Мини-график с осью Y ====================== */
function MiniBarsWithAxis({ data }: { data: number[] }) {
  const gradientId = useId();
  const w = 560;
  const h = 88;
  const pad = 8;
  const yAxisW = 46;
  const plotW = w - pad * 2 - yAxisW;
  const plotH = h - pad * 2;
  const max = Math.max(...data, 1);
  const barW = plotW / data.length;

  const ticks = [0, 1 / 3, 2 / 3, 1].map((t) => Math.round(t * max));

  return (
    <div className="mt-3">
      <svg viewBox={`0 0 ${w} ${h}`} className="w-full h-[88px]">
        {ticks.map((val, i) => {
          const y = h - pad - (val / max) * plotH;
          return (
            <g key={i}>
              <line x1={yAxisW} y1={y} x2={w - pad} y2={y} stroke="rgba(255,255,255,0.10)" strokeWidth="1" />
              <text
                x={yAxisW - 6}
                y={y}
                fontSize="11"
                textAnchor="end"
                dominantBaseline="central"
                fill="rgba(203,213,225,0.95)"
              >
                {compact(val)}
              </text>
            </g>
          );
        })}

        {data.map((v, i) => {
          const bh = Math.max(2, (v / max) * plotH);
          const x = yAxisW + i * barW + 1;
          const y = h - pad - bh;
          return (
            <rect
              key={i}
              x={x}
              y={y}
              width={Math.max(2, barW - 2)}
              height={bh}
              rx="2"
              ry="2"
              fill={`url(#g-${gradientId})`}
            />
          );
        })}

        <text x={yAxisW} y={h - 2} fontSize="11" fill="rgba(148,163,184,0.95)">
          24 часа
        </text>
        <text x={w - pad} y={h - 2} fontSize="11" textAnchor="end" fill="rgba(148,163,184,0.95)">
          ср.: {compact(Math.round(data.reduce((a, b) => a + b, 0) / data.length))}/ч
        </text>

        <defs>
          <linearGradient id={`g-${gradientId}`} x1="0" x2="0" y1="0" y2="1">
            <stop offset="0%" stopColor="#67d3ff" />
            <stop offset="100%" stopColor="#57b6ff" />
          </linearGradient>
        </defs>
      </svg>
    </div>
  );
}

/* ====================== Модалка отзывов ====================== */
function ReviewsModal({
  service,
  onClose,
  onStats,
}: {
  service: ServiceSummary;
  onClose: () => void;
  onStats: (serviceId: string, stats: { averageRating: number; reviewCount: number }) => void;
}) {
  const [list, setList] = useState<ServiceReview[]>([]);
  const [rate, setRate] = useState<number>(5);
  const [text, setText] = useState<string>("");
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<Error | null>(null);
  const [submitting, setSubmitting] = useState<boolean>(false);
  const [submitError, setSubmitError] = useState<Error | null>(null);
  const statsRef = useRef(onStats);

  useEffect(() => {
    statsRef.current = onStats;
  }, [onStats]);

  // esc для закрытия, блокировка прокрутки тела
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    document.addEventListener("keydown", onKey);
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.removeEventListener("keydown", onKey);
      document.body.style.overflow = prev;
    };
  }, [onClose]);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    setSubmitError(null);
    setList([]);
    setRate(5);
    setText("");

    void api
      .listServiceReviews(service.id)
      .then((data) => {
        if (cancelled) return;
        setList(data.reviews);
        statsRef.current?.(service.id, {
          averageRating: data.averageRating,
          reviewCount: data.reviewCount,
        });
      })
      .catch((err) => {
        if (cancelled) return;
        const e = err instanceof Error ? err : new Error("Не удалось загрузить отзывы");
        setError(e);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [service.id]);

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || submitting) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      const result = await api.createServiceReview(service.id, {
        rating: rate,
        text: trimmed,
      });
      setList((prev) => [result.review, ...prev]);
      setText("");
      statsRef.current?.(service.id, {
        averageRating: result.averageRating,
        reviewCount: result.reviewCount,
      });
    } catch (err) {
      const e = err instanceof Error ? err : new Error("Не удалось отправить отзыв");
      setSubmitError(e);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 grid place-items-center bg-black/60 p-3"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
    >
      <div
        className="w-[min(720px,100%)] rounded-2xl border border-white/10 bg-slate-900/70 backdrop-blur-xl p-4 text-slate-200 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <div className="text-lg font-semibold">Отзывы: {service.name}</div>
          <button
            onClick={onClose}
            className="rounded-xl border border-white/10 bg-slate-900/55 hover:bg-slate-900/70 backdrop-blur-xl px-3 py-1.5 text-sm"
          >
            Закрыть
          </button>
        </div>

        {/* Форма */}
        <div className="rounded-xl border border-white/10 bg-white/5 p-3">
          <div className="mb-2 text-sm text-slate-300">Ваша оценка</div>
          <div className="mb-3 flex items-center gap-1">
            {Array.from({ length: 5 }, (_, i) => {
              const v = i + 1;
              return (
                <button
                  key={v}
                  onClick={() => setRate(v)}
                  className={`text-xl leading-none transition ${
                    v <= rate ? "text-yellow-300" : "text-slate-400/60"
                  }`}
                  aria-label={`${v} звезд`}
                >
                  ★
                </button>
              );
            })}
            <span className="ml-2 text-slate-400">{rate}/5</span>
          </div>

          <textarea
            value={text}
            onChange={(e) => setText(e.target.value)}
            placeholder="Опишите проблему или успех…"
            className="w-full rounded-xl border border-white/10 bg-slate-900/55 backdrop-blur-xl p-3 outline-none placeholder:text-slate-400"
            rows={4}
          />

          <div className="mt-3 flex flex-col items-end gap-2">
            {submitError && (
              <div className="w-full rounded-xl border border-rose-500/40 bg-rose-500/10 px-3 py-2 text-sm text-rose-200">
                {submitError.message}
              </div>
            )}
            <button
              onClick={submit}
              disabled={submitting || !text.trim()}
              className={`inline-flex items-center gap-2 rounded-2xl border border-white/10 bg-slate-900/55 px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg transition ${
                submitting || !text.trim() ? "cursor-not-allowed opacity-60" : "hover:bg-slate-900/70"
              }`}
            >
              {submitting ? "Отправляем…" : "Отправить"}
            </button>
          </div>
        </div>

        {/* Список отзывов */}
        <div className="mt-4 max-h-[50vh] space-y-3 overflow-y-auto pr-1">
          {loading && <div className="text-slate-400">Загружаем отзывы…</div>}
          {error && (
            <div className="rounded-xl border border-rose-500/40 bg-rose-500/10 p-3 text-sm text-rose-200">
              {error.message}
            </div>
          )}
          {!loading && !error && list.length === 0 && (
            <div className="text-slate-400">Пока нет отзывов — будьте первым.</div>
          )}
          {!error &&
            list.map((r) => (
              <div key={r.id} className="rounded-xl border border-white/10 bg-white/5 p-3">
                <div className="mb-1 text-yellow-300">
                  {Array.from({ length: r.rating }).map((_, k) => (
                    <span key={k}>★</span>
                  ))}
                  {Array.from({ length: 5 - r.rating }).map((_, k) => (
                    <span key={k} className="text-slate-400/50">
                      ★
                    </span>
                  ))}
                </div>
                <div className="whitespace-pre-wrap">{r.text}</div>
                <div className="mt-1 text-xs text-slate-400">
                  {new Date(r.createdAt).toLocaleString()}
                </div>
              </div>
            ))}
        </div>
      </div>
    </div>
  );
}
