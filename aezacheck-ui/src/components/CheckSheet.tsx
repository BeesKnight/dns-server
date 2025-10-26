import { useEffect, useMemo, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Globe2, ChevronUp, ChevronDown } from "lucide-react";
import { api, type CheckResult } from "../lib/api";
import { useConnection } from "../store/connection";

type Props = {
  open: boolean;
  onOpenChange: (v: boolean) => void;
};

type DnsRecord = {
  type: string;
  name?: string;
  ttl?: number;
  value: string;
};

type DnsRecordGroup = {
  type: string;
  records: DnsRecord[];
};

const TERMINAL_STATUSES = new Set(["done", "error", "cancelled", "unknown"]);
const POLL_INTERVAL_MS = 2500;
const POLL_ERROR_INTERVAL_MS = 5000;

const formatStatus = (status: string | null): string => {
  if (!status) return "—";
  switch (status.toLowerCase()) {
    case "queued":
      return "В очереди";
    case "running":
      return "Выполняется";
    case "done":
      return "Завершено";
    case "cancelled":
      return "Отменено";
    case "error":
      return "Ошибка";
    default:
      return status;
  }
};

const parseTTL = (value: unknown): number | undefined => {
  if (typeof value === "number" && Number.isFinite(value)) {
    return Math.round(value);
  }
  if (typeof value === "string") {
    const n = Number(value.trim());
    if (Number.isFinite(n)) {
      return Math.round(n);
    }
  }
  return undefined;
};

const normalizeAnswerValue = (value: unknown): string | undefined => {
  if (value === null || value === undefined) return undefined;
  if (Array.isArray(value)) {
    const parts = value
      .map((entry) => normalizeAnswerValue(entry))
      .filter((part): part is string => typeof part === "string" && part.length > 0);
    if (parts.length > 0) {
      return parts.join(" ");
    }
    return undefined;
  }
  if (typeof value === "string") {
    const trimmed = value.trim();
    return trimmed ? trimmed : undefined;
  }
  if (typeof value === "number") {
    return Number.isFinite(value) ? String(value) : undefined;
  }
  if (typeof value === "boolean") {
    return value ? "true" : "false";
  }
  return undefined;
};

const formatAnswerValue = (answer: Record<string, unknown>): string => {
  const prioritizedKeys = [
    "data",
    "value",
    "address",
    "exchange",
    "target",
    "content",
    "cname",
    "ptrdname",
    "nsdname",
    "txt",
    "text",
    "strings",
    "ipv4",
    "ipv6",
    "ip",
    "host",
    "answer",
  ];
  for (const key of prioritizedKeys) {
    const normalized = normalizeAnswerValue(answer[key]);
    if (normalized) {
      return normalized;
    }
  }
  for (const val of Object.values(answer)) {
    const normalized = normalizeAnswerValue(val);
    if (normalized) {
      return normalized;
    }
  }
  try {
    return JSON.stringify(answer);
  } catch {
    return String(answer);
  }
};

const extractDnsRecords = (payload: unknown): DnsRecord[] => {
  if (!payload || typeof payload !== "object") return [];
  const answers = (payload as { answers?: unknown }).answers;
  if (!Array.isArray(answers)) return [];

  const records: DnsRecord[] = [];
  for (const raw of answers) {
    if (!raw || typeof raw !== "object") continue;
    const answer = raw as Record<string, unknown>;
    const typeSource =
      answer["type"] ??
      answer["rrtype"] ??
      answer["record_type"] ??
      answer["rtype"] ??
      answer["Type"];

    let type = "";
    if (typeof typeSource === "string") {
      type = typeSource.toUpperCase();
    } else if (typeof typeSource === "number") {
      type = String(typeSource);
    }
    if (!type) continue;

    const value = formatAnswerValue(answer);
    if (!value) continue;

    const ttl = parseTTL(
      answer["ttl"] ?? answer["TTL"] ?? answer["time_to_live"] ?? answer["max_ttl"]
    );
    const nameSource =
      answer["name"] ??
      answer["host"] ??
      answer["owner"] ??
      answer["domain"] ??
      answer["fqdn"];
    const name = typeof nameSource === "string" ? nameSource : undefined;

    records.push({ type, value, ttl, name });
  }
  return records;
};

export default function CheckSheet({ open, onOpenChange }: Props) {
  const [url, setUrl] = useState("");
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [activeCheckId, setActiveCheckId] = useState<string | null>(null);
  const [checkStatus, setCheckStatus] = useState<string | null>(null);
  const [dnsServer, setDnsServer] = useState<string | null>(null);
  const [results, setResults] = useState<CheckResult[]>([]);
  const [resultsError, setResultsError] = useState<string | null>(null);
  const [polling, setPolling] = useState(false);
  const [lastCheckUrl, setLastCheckUrl] = useState<string | null>(null);

  const { ip, geo, loading: connectionLoading, error: connectionError } = useConnection();

  useEffect(() => {
    if (!activeCheckId) return;

    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const poll = async () => {
      setPolling(true);
      let nextDelay: number | null = null;

      try {
        const data = await api.getCheckResults(activeCheckId);
        if (cancelled) return;
        setResults(data.results);
        setDnsServer(data.dnsServer?.trim() || null);
        setCheckStatus(data.status?.trim() || null);
        setResultsError(null);

        const normalized = (data.status ?? "").toLowerCase();
        if (!TERMINAL_STATUSES.has(normalized)) {
          nextDelay = POLL_INTERVAL_MS;
        }
      } catch (error: unknown) {
        if (cancelled) return;
        const message =
          error instanceof Error
            ? error.message
            : "Не удалось получить результаты";
        setResultsError(message);
        nextDelay = POLL_ERROR_INTERVAL_MS;
      } finally {
        if (!cancelled) {
          setPolling(false);
          if (nextDelay !== null) {
            timer = setTimeout(poll, nextDelay);
          }
        }
      }
    };

    poll();

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [activeCheckId]);

  const dnsRecords = useMemo<DnsRecord[]>(() => {
    if (results.length === 0) return [];
    const collected: DnsRecord[] = [];
    for (const entry of results) {
      if (entry.kind !== "dns") continue;
      collected.push(...extractDnsRecords(entry.payload));
    }
    return collected;
  }, [results]);

  const dnsGroups = useMemo<DnsRecordGroup[]>(() => {
    if (dnsRecords.length === 0) return [];
    const grouped = new Map<string, DnsRecord[]>();
    for (const record of dnsRecords) {
      const key = record.type || "OTHER";
      const existing = grouped.get(key);
      if (existing) {
        existing.push(record);
      } else {
        grouped.set(key, [record]);
      }
    }
    return Array.from(grouped.entries()).map(([type, records]) => ({ type, records }));
  }, [dnsRecords]);

  const normalizedStatus = (checkStatus ?? "").toLowerCase();
  const isTerminal = normalizedStatus
    ? TERMINAL_STATUSES.has(normalizedStatus)
    : false;

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

  async function runQuick() {
    setErr(null);
    if (!url.trim()) {
      setErr("Укажи домен или URL");
      return;
    }
    try {
      setLoading(true);
      const trimmed = url.trim();
      const { check_id: checkId } = await api.startQuickCheck(trimmed);
      setLastCheckUrl(trimmed);
      setResults([]);
      setResultsError(null);
      setDnsServer(null);
      setCheckStatus("queued");
      setActiveCheckId(checkId);
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
                      События в реальном времени: <code>check.start</code>,{" "}
                      <code>check.done</code>, <code>agents</code>.
                    </div>
                  </div>
                </div>

                {activeCheckId && (
                  <>
                    <div className="h-px w-full bg-white/5" />
                    <div className="px-6 pb-6 pt-5">
                      <div className="flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
                        <div>
                          <div className="text-xs uppercase tracking-wide text-slate-400">
                            Активная проверка
                          </div>
                          <div className="mt-1 break-words text-sm font-medium text-slate-100">
                            {lastCheckUrl ?? "—"}
                          </div>
                          <div className="mt-2 text-[11px] text-slate-500">
                            ID:{" "}
                            <code className="text-slate-300">{activeCheckId}</code>
                          </div>
                        </div>
                        <div className="space-y-1 text-xs text-slate-400 md:text-right">
                          <div>
                            Статус:{" "}
                            <span className="text-slate-200">{formatStatus(checkStatus)}</span>
                          </div>
                          {polling && (
                            <div className="text-[11px] text-blue-300">Обновление результатов…</div>
                          )}
                          {dnsServer && (
                            <div>
                              DNS сервер:{" "}
                              <code className="text-slate-200">{dnsServer}</code>
                            </div>
                          )}
                        </div>
                      </div>

                      <div className="mt-4">
                        <div className="text-xs uppercase tracking-wide text-slate-400">
                          Полученные результаты
                        </div>
                        {results.length > 0 ? (
                          <div className="mt-2 flex flex-wrap gap-2">
                            {results.map((result) => (
                              <span
                                key={result.id}
                                className="rounded-full border border-white/10 bg-slate-800/60 px-3 py-1 text-xs text-slate-200"
                              >
                                <span className="font-semibold text-slate-100">
                                  {result.kind.toUpperCase()}
                                </span>
                                <span className="text-slate-400"> · {result.status}</span>
                              </span>
                            ))}
                          </div>
                        ) : (
                          <div className="mt-2 text-xs text-slate-500">
                            Ожидаем первые ответы агентов.
                          </div>
                        )}
                      </div>

                      {resultsError && (
                        <div className="mt-4 rounded-lg border border-yellow-500/30 bg-yellow-500/10 px-3 py-2 text-xs text-yellow-200">
                          {resultsError}
                        </div>
                      )}

                      <div className="mt-5">
                        <div className="text-sm font-medium text-slate-200">DNS ответы</div>
                        {dnsGroups.length > 0 ? (
                          <div className="mt-3 grid gap-3 md:grid-cols-2">
                            {dnsGroups.map((group) => (
                              <div
                                key={group.type}
                                className="rounded-xl border border-white/10 bg-slate-800/40 p-3"
                              >
                                <div className="flex items-center justify-between">
                                  <div className="text-xs uppercase tracking-wide text-blue-200">
                                    {group.type}
                                  </div>
                                  <div className="text-[11px] text-slate-500">
                                    {group.records.length}{" "}
                                    {group.records.length === 1 ? "запись" : "записи"}
                                  </div>
                                </div>
                                <div className="mt-3 space-y-2">
                                  {group.records.map((record, index) => (
                                    <div
                                      key={`${group.type}-${index}-${record.value}`}
                                      className="rounded-lg bg-slate-900/40 px-3 py-2"
                                    >
                                      <div className="break-words font-mono text-sm text-slate-100">
                                        {record.value}
                                      </div>
                                      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-400">
                                        {record.name && <span>{record.name}</span>}
                                        {typeof record.ttl === "number" && <span>TTL {record.ttl}s</span>}
                                      </div>
                                    </div>
                                  ))}
                                </div>
                              </div>
                            ))}
                          </div>
                        ) : (
                          <div className="mt-2 text-xs text-slate-500">
                            {isTerminal
                              ? "DNS ответы не получены."
                              : "Данные появятся после завершения DNS-проверки."}
                          </div>
                        )}
                      </div>
                    </div>
                  </>
                )}
              </div>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </>
  );
}
