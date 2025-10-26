import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";

import { api, type ProfileData } from "../lib/api";
import { useAuth } from "../store/auth";

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

const formatNumber = (value: number) =>
  new Intl.NumberFormat("ru-RU", { maximumFractionDigits: 0 }).format(value);

const stars = (rating: number) =>
  Array.from({ length: 5 }, (_, index) => (index < Math.round(rating) ? "★" : "☆"));

export default function Profile() {
  const { user } = useAuth();
  const [profile, setProfile] = useState<ProfileData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const dateTimeFormatter = useMemo(
    () =>
      new Intl.DateTimeFormat("ru-RU", {
        dateStyle: "medium",
        timeStyle: "short",
      }),
    []
  );

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);

    void api
      .getProfile()
      .then((data) => {
        if (!cancelled) setProfile(data);
      })
      .catch((err) => {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : "Не удалось загрузить профиль";
        setError(message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, []);

  const lastCheck = profile?.lastCheckAt
    ? dateTimeFormatter.format(new Date(profile.lastCheckAt))
    : "—";

  return (
    <div className="relative min-h-screen overflow-hidden text-white">
      <BgFX />

      <div className="relative mx-auto flex min-h-screen max-w-5xl flex-col px-4 pb-10 pt-12">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <Link
            to="/app"
            className="inline-flex items-center gap-2 rounded-2xl border border-white/10 bg-black/30 px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg backdrop-blur-md transition hover:bg-white/10"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <path d="M15 19l-7-7 7-7" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            К карте
          </Link>

          <div className="flex flex-wrap items-center gap-2">
            <Link
              to="/app/services"
              className="inline-flex items-center gap-2 rounded-2xl border border-white/10 bg-black/30 px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg backdrop-blur-md transition hover:bg-white/10"
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <rect x="3" y="3" width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6" />
                <rect x="14" y="3" width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6" />
                <rect x="3" y="14" width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6" />
                <rect x="14" y="14" width="7" height="7" rx="2" stroke="currentColor" strokeWidth="1.6" />
              </svg>
              Сервисы
            </Link>
            <Link
              to="/app/agents"
              className="inline-flex items-center gap-2 rounded-2xl border border-white/10 bg-black/30 px-3 py-2 text-sm font-semibold text-slate-200 shadow-lg backdrop-blur-md transition hover:bg-white/10"
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path d="M4 5h16M4 10h16M4 15h10" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
                <circle cx="18" cy="15" r="1.5" stroke="currentColor" strokeWidth="1.6" />
              </svg>
              Агенты
            </Link>
          </div>
        </div>

        <div className="mt-8 grid flex-1 gap-6 lg:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
          <section className="flex flex-col rounded-3xl border border-white/10 bg-black/35 p-6 shadow-2xl backdrop-blur-xl">
            <header className="flex flex-col gap-2 border-b border-white/10 pb-4">
              <span className="text-xs uppercase tracking-wide text-slate-400">Аккаунт</span>
              <h1 className="text-3xl font-semibold">{user?.email ?? "Ваш профиль"}</h1>
              <p className="text-sm text-slate-300">
                Последняя проверка: <span className="font-semibold text-white">{lastCheck}</span>
              </p>
            </header>

            <div className="mt-5">
              <div className="mb-4 text-sm text-slate-300">Последние отзывы</div>

              {loading && (
                <div className="space-y-3">
                  {Array.from({ length: 3 }).map((_, index) => (
                    <div
                      key={index}
                      className="h-20 animate-pulse rounded-2xl border border-white/5 bg-white/5"
                    />
                  ))}
                </div>
              )}

              {!loading && error && (
                <div className="rounded-2xl border border-rose-500/40 bg-rose-500/15 px-4 py-3 text-sm text-rose-100">
                  {error}
                </div>
              )}

              {!loading && !error && profile && profile.reviews.length === 0 && (
                <div className="rounded-2xl border border-white/10 bg-white/5 px-4 py-4 text-sm text-slate-300">
                  У вас пока нет отзывов. Оставляйте первые впечатления о сервисах!
                </div>
              )}

              {!loading && !error && profile && profile.reviews.length > 0 && (
                <ul className="grid gap-3">
                  {profile.reviews.map((review) => (
                    <li
                      key={review.id}
                      className="rounded-2xl border border-white/10 bg-white/5 px-4 py-4 backdrop-blur-sm transition hover:border-white/20 hover:bg-white/10"
                    >
                      <div className="flex flex-wrap items-center justify-between gap-3">
                        <div className="min-w-0">
                          <div className="text-sm text-slate-300">{review.serviceName}</div>
                          <div className="font-semibold text-white">
                            {stars(review.rating).map((symbol, index) => (
                              <span key={index} className={symbol === "☆" ? "opacity-30" : ""}>
                                {symbol}
                              </span>
                            ))}
                            <span className="ml-2 text-sm text-slate-300">{review.rating.toFixed(1)}</span>
                          </div>
                        </div>
                        <div className="text-right text-xs text-slate-400">
                          {dateTimeFormatter.format(new Date(review.createdAt))}
                        </div>
                      </div>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </section>

          <aside className="flex flex-col gap-4 rounded-3xl border border-white/10 bg-black/35 p-6 shadow-2xl backdrop-blur-xl">
            <div>
              <div className="text-xs uppercase tracking-wide text-slate-400">Статистика</div>
              <div className="mt-3 text-4xl font-semibold text-white">
                {profile ? formatNumber(profile.totalReviews) : "—"}
              </div>
              <div className="text-sm text-slate-300">Всего отзывов</div>
            </div>

            <div className="rounded-2xl border border-white/10 bg-white/5 p-4 text-sm text-slate-300">
              Здесь будут появляться накопленные бейджи и достижения. Следите за обновлениями!
            </div>
          </aside>
        </div>
      </div>
    </div>
  );
}
