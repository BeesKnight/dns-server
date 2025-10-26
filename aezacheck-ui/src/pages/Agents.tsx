import { Link } from "react-router-dom";
import AgentDashboard from "../components/AgentDashboard";

export default function AgentsPage() {
  return (
    <div className="min-h-screen bg-slate-950 px-4 py-6 text-white">
      <div className="mb-6 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <Link
            to="/app"
            className="inline-flex items-center gap-2 rounded-2xl border border-white/10 bg-slate-900/60 px-3 py-2 text-sm font-semibold text-slate-200 transition hover:bg-slate-800/60"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <path d="M15 19l-7-7 7-7" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            К карте
          </Link>
          <h1 className="text-2xl font-bold">Консоль агентов</h1>
        </div>
        <Link
          to="/app/services"
          className="inline-flex items-center gap-2 rounded-2xl border border-white/10 bg-slate-900/60 px-3 py-2 text-sm font-semibold text-slate-200 transition hover:bg-slate-800/60"
        >
          Популярные сервисы
        </Link>
      </div>
      <AgentDashboard />
    </div>
  );
}
