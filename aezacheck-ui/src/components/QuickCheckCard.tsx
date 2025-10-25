import { useState } from "react";
import { Globe, Loader2 } from "lucide-react";
import { api } from "../lib/api";

export default function QuickCheckCard() {
  const [url, setUrl] = useState("");
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    if (!url.trim()) {
      setErr("Введите домен или URL");
      return;
    }
    try {
      setLoading(true);
      // минимальный «quick» шаблон: http + ping + dns
      await api.startQuickCheck(url.trim());
      // дальше можно показать тост/статус, подписаться на SSE и т.д.
    } catch (e: any) {
      setErr(e?.message || "Не удалось запустить проверку");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="glass w-full max-w-[520px] rounded-3xl p-6 md:p-8 shadow-2xl">
      <div className="mb-4 flex items-center gap-3">
        <div className="flex size-9 items-center justify-center rounded-2xl bg-white/10">
          <Globe className="size-5 text-blue-300" />
        </div>
        <div className="text-lg font-semibold text-white">Проверить сайт</div>
      </div>

      <form onSubmit={submit} className="space-y-3">
        <label className="label">Домен или URL</label>
        <input
          className="input w-full"
          type="text"
          placeholder="https://example.com"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
        />

        <p className="text-xs text-slate-400">
          Шаблон quick: HTTPS GET + ping + DNS.
        </p>

        {err && (
          <div className="rounded-xl border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-200">
            {err}
          </div>
        )}

        <button
          type="submit"
          disabled={loading}
          className="mt-2 w-full rounded-2xl bg-blue-600 py-3 font-medium text-white shadow-lg shadow-blue-500/20 hover:bg-blue-700 disabled:opacity-60"
        >
          {loading ? (
            <span className="inline-flex items-center gap-2">
              <Loader2 className="size-4 animate-spin" />
              Запуск…
            </span>
          ) : (
            "Запустить проверку"
          )}
        </button>
      </form>
    </div>
  );
}
