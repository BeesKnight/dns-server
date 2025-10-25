import { useEffect, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Mail, Lock, Eye, EyeOff, ShieldCheck } from "lucide-react";
import { useAuth } from "../store/auth";
import GooglyShapes from "../components/GooglyShapes";
import { useNavigate } from "react-router-dom";

export default function AuthPage() {
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [pass, setPass] = useState("");
  const [show, setShow] = useState(false);

  // важно: тот же union, что ждёт GooglyShapes
  const [focus, setFocus] = useState<"none" | "email" | "password">("none");

  const { user, loading, error, login, register, bootstrap } = useAuth();
  const navigate = useNavigate();

  useEffect(() => { bootstrap(); }, [bootstrap]);
  useEffect(() => {
    if (user) navigate("/app", { replace: true }); // после входа → /app
  }, [user, navigate]);

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (mode === "login") await login(email, pass);
    else await register(email, pass);
  }

  return (
    <div className="relative min-h-screen overflow-hidden">
      {/* градиенты фона */}
      <BgGradients />

      {/* слой фигур с «прищуром» */}
      <GooglyShapes focusState={focus} />

      {/* основная раскладка */}
      <div className="relative z-40 grid min-h-screen grid-cols-1 md:grid-cols-2">
        {/* левая панель */}
        <div className="hidden md:flex items-center justify-center">
          <div className="px-10">
            <motion.h1
              initial={{ y: 20, opacity: 0 }}
              animate={{ y: 0, opacity: 1 }}
              transition={{ duration: 0.6 }}
              className="text-4xl font-semibold tracking-tight mb-4"
            >
              AEZA Check
            </motion.h1>
            <motion.p
              initial={{ y: 20, opacity: 0 }}
              animate={{ y: 0, opacity: 1 }}
              transition={{ duration: 0.6, delay: 0.05 }}
              className="text-slate-300 max-w-md"
            >
              Мониторинг HTTP · TCP · Ping · Trace · DNS. Реал-тайм события на карте, кэш 30с.
            </motion.p>

            <div className="mt-8 flex items-center gap-3 text-slate-300/80">
              <ShieldCheck className="size-5 text-emerald-400" />
              <span className="text-sm">JWT, HTTPS, CORS.</span>
            </div>
          </div>
        </div>

        {/* правая панель (форма) */}
        <div className="flex items-center justify-center p-6">
          <motion.div
            initial={{ y: 24, opacity: 0 }}
            animate={{ y: 0, opacity: 1 }}
            className="glass w-full max-w-md rounded-3xl p-8"
          >
            {/* переключатель */}
            <div className="mb-6 flex rounded-2xl bg-white/5 p-1">
              {(["login", "register"] as const).map((m) => (
                <button
                  key={m}
                  onClick={() => setMode(m)}
                  className={`relative w-1/2 py-2 text-sm font-medium ${
                    mode === m ? "text-white" : "text-slate-300"
                  }`}
                >
                  <span className="capitalize">{m === "login" ? "Login" : "Sign Up"}</span>
                  {mode === m && (
                    <motion.span
                      layoutId="auth-pill"
                      className="absolute inset-0 -z-10 rounded-xl bg-white/10"
                      transition={{ type: "spring", stiffness: 400, damping: 30 }}
                    />
                  )}
                </button>
              ))}
            </div>

            <form onSubmit={onSubmit} className="space-y-4">
              <Field label="Email" icon={<Mail className="size-4" />}>
                <input
                  className="input"
                  type="email"
                  placeholder="you@example.com"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                  onFocus={() => setFocus("email")}
                  onBlur={() => setFocus("none")}
                  required
                />
              </Field>

              <Field label="Password" icon={<Lock className="size-4" />}>
                <div className="relative">
                  <input
                    className="input pr-10"
                    type={show ? "text" : "password"}
                    placeholder="••••••••"
                    value={pass}
                    onChange={(e) => setPass(e.target.value)}
                    onFocus={() => setFocus("password")}
                    onBlur={() => setFocus("none")}
                    minLength={6}
                    required
                  />
                  <button
                    type="button"
                    onClick={() => setShow((s) => !s)}
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-slate-300 hover:text-white"
                    aria-label={show ? "Hide password" : "Show password"}
                  >
                    {show ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
                  </button>
                </div>
              </Field>

              <AnimatePresence>
                {mode === "register" && (
                  <motion.div
                    initial={{ height: 0, opacity: 0 }}
                    animate={{ height: "auto", opacity: 1 }}
                    exit={{ height: 0, opacity: 0 }}
                    className="text-xs text-slate-300/80"
                  >
                    Регистрируясь, вы соглашаетесь с правилами сервиса.
                  </motion.div>
                )}
              </AnimatePresence>

              {error && (
                <div className="rounded-xl bg-red-500/15 border border-red-500/30 px-3 py-2 text-sm text-red-200">
                  {error}
                </div>
              )}

              <button
                disabled={loading}
                className="w-full rounded-2xl bg-blue-600 hover:bg-blue-700 py-3 font-medium shadow-lg shadow-blue-500/20 disabled:opacity-60"
              >
                {loading ? "Please wait…" : mode === "login" ? "Sign in" : "Create account"}
              </button>
            </form>
          </motion.div>
        </div>
      </div>
    </div>
  );
}

function Field({
  label,
  icon,
  children,
}: {
  label: string;
  icon?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="label">{label}</span>
      <div className="relative">
        {icon && (
          <span className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-slate-300">
            {icon}
          </span>
        )}
        <div className={icon ? "pl-9" : ""}>{children}</div>
      </div>
    </label>
  );
}

function BgGradients() {
  return (
    <>
      <div className="pointer-events-none absolute inset-0 bg-[radial-gradient(60%_50%_at_100%_0%,rgba(59,130,246,0.35),transparent_60%),radial-gradient(60%_50%_at_0%_100%,rgba(99,102,241,0.35),transparent_60%)]" />
      <div className="pointer-events-none absolute -top-32 -right-32 h-72 w-72 rounded-full bg-white/10 blur-3xl" />
      <div className="pointer-events-none absolute -bottom-16 -left-16 h-64 w-64 rounded-full bg-white/10 blur-3xl" />
    </>
  );
}
