import { useEffect, useRef, useState } from "react";
import { motion } from "framer-motion";

type FocusKind = "none" | "email" | "password";

/** Один глаз: зрачок следует за курсором; ВЕКИ (чёрные) управляются через открытость aperture */
function Eye({
  size = 56,
  pupil = 18,
  maxMove = 10,
  aperture = 1, // 1 — открыт, 0 — закрыт
  className = "",
}: {
  size?: number;
  pupil?: number;
  maxMove?: number;
  aperture?: number;
  className?: string;
}) {
  const eyeRef = useRef<HTMLDivElement>(null);
  const pupilRef = useRef<HTMLDivElement>(null);
  const topLidRef = useRef<HTMLDivElement>(null);
  const botLidRef = useRef<HTMLDivElement>(null);

  // зрачок следует за курсором (без setState)
  useEffect(() => {
    const eye = eyeRef.current!;
    const pu = pupilRef.current!;
    if (!eye || !pu) return;

    const onMove = (e: MouseEvent) => {
      const r = eye.getBoundingClientRect();
      const cx = r.left + r.width / 2;
      const cy = r.top + r.height / 2;
      const dx = e.clientX - cx;
      const dy = e.clientY - cy;
      const d = Math.hypot(dx, dy) || 1;
      const k = Math.min(maxMove, d) / d;
      pu.style.transform = `translate(calc(-50% + ${dx * k}px), calc(-50% + ${dy * k}px))`;
    };

    const handler = (e: MouseEvent) => requestAnimationFrame(() => onMove(e));
    window.addEventListener("mousemove", handler);
    return () => window.removeEventListener("mousemove", handler);
  }, [maxMove]);

  // применяем открытость глаза к векам (обеим сразу)
  useEffect(() => {
    const t = topLidRef.current!;
    const b = botLidRef.current!;
    if (!t || !b) return;
    t.style.transition = "height 140ms ease";
    b.style.transition = "height 140ms ease";
    const h = (1 - Math.max(0, Math.min(1, aperture))) * 50; // 0..50%
    t.style.height = `${h}%`;
    b.style.height = `${h}%`;
  }, [aperture]);

  return (
    <div
      ref={eyeRef}
      className={`relative rounded-full bg-white shadow-inner border border-black/10 ${className}`}
      style={{ width: size, height: size }}
    >
      {/* глазное яблоко (клип для содержимого) */}
      <div className="absolute inset-0 rounded-full overflow-hidden">
        {/* зрачок */}
        <div
          ref={pupilRef}
          className="absolute left-1/2 top-1/2 z-0 rounded-full bg-black shadow-sm"
          style={{ width: pupil, height: pupil, transform: "translate(-50%, -50%)" }}
        />
        {/* ВЕРХНЕЕ/НИЖНЕЕ ВЕКО — теперь ЧЁРНЫЕ */}
        <div
          ref={topLidRef}
          className="absolute left-0 top-0 z-10 w-full bg-black"
          style={{ height: "0%", borderTopLeftRadius: 9999, borderTopRightRadius: 9999 }}
        />
        <div
          ref={botLidRef}
          className="absolute left-0 bottom-0 z-10 w-full bg-black"
          style={{ height: "0%", borderBottomLeftRadius: 9999, borderBottomRightRadius: 9999 }}
        />
      </div>

      {/* блик */}
      <div
        className="pointer-events-none absolute left-1/2 top-1/2 rounded-full bg-white/70"
        style={{
          width: Math.max(6, pupil * 0.35),
          height: Math.max(6, pupil * 0.35),
          transform: "translate(-10px, -12px)",
          filter: "blur(1px)",
        }}
      />
    </div>
  );
}

/** Фигура с двумя глазами. Здесь делаем ОБЩЕЕ моргание для обоих глаз сразу */
function ShapeWithEyes({
  w,
  h,
  radius = 24,
  rotate = 0,
  gradient = "from-sky-500/60 to-blue-400/40",
  className = "",
  eyesGap = 12,
  eyeSize = 56,
  aperture = 1,
  maxMove = 10,
}: {
  w: number;
  h: number;
  radius?: number;
  rotate?: number;
  gradient?: string;
  className?: string;
  eyesGap?: number;
  eyeSize?: number;
  aperture?: number;
  maxMove?: number;
}) {
  // коэффициент моргания для обоих глаз: 1 — открыт, 0 — закрыт
  const [blink, setBlink] = useState(1);

  useEffect(() => {
    let t1: number, t2: number, t3: number, t4: number;

    const schedule = () => {
      // случайная пауза между морганиями (2.6s..7.6s)
      const wait = 2600 + Math.random() * 5000;
      t1 = window.setTimeout(() => {
        setBlink(0); // закрыть оба глаза
        t2 = window.setTimeout(() => {
          setBlink(1); // открыть до базового прищура
          // шанс «двойного» морга
          if (Math.random() < 0.22) {
            t3 = window.setTimeout(() => {
              setBlink(0);
              t4 = window.setTimeout(() => {
                setBlink(1);
                schedule();
              }, 120);
            }, 180);
          } else {
            schedule();
          }
        }, 140);
      }, wait);
    };

    schedule();
    return () => {
      window.clearTimeout(t1);
      window.clearTimeout(t2);
      window.clearTimeout(t3);
      window.clearTimeout(t4);
    };
  }, [aperture]);

  const open = Math.max(0, Math.min(1, aperture * blink));

  return (
    <motion.div
      className={`pointer-events-none relative ${className}`}
      style={{ width: w, height: h }}
      initial={{ y: 0 }}
      animate={{ y: [0, -8, 0] }}
      transition={{ duration: 6, repeat: Infinity, ease: "easeInOut" }}
    >
      <div
        className={`absolute inset-0 bg-gradient-to-br ${gradient} shadow-2xl ring-1 ring-white/25`}
        style={{ borderRadius: radius, transform: `rotate(${rotate}deg)` }}
      />
      <div
        className="absolute left-1/2 top-1/2 flex -translate-x-1/2 -translate-y-1/2 items-center"
        style={{ gap: eyesGap }}
      >
        <Eye size={eyeSize} aperture={open} maxMove={maxMove} />
        <Eye size={eyeSize} aperture={open} maxMove={maxMove} />
      </div>
    </motion.div>
  );
}

/** Слой с фигурами; focusState управляет прищуром (aperture) */
export default function GooglyShapes({ focusState }: { focusState: FocusKind }) {
  const baseAperture =
    focusState === "password" ? 0.45 : focusState === "email" ? 0.7 : 1;
  const move = focusState === "password" ? 6 : focusState === "email" ? 8 : 10;

  return (
    <div className="fixed inset-0 z-30 pointer-events-none">
      {/* ВЕРХНИЙ КВАДРАТ — левее и выше центра */}
      <div
        className="absolute"
        style={{ left: "50%", top: "50%", transform: "translate(-150px,-210px)" }}
      >
        <ShapeWithEyes
          w={220}
          h={220}
          rotate={-8}
          gradient="from-sky-500/60 to-blue-400/40"
          eyeSize={54}
          aperture={baseAperture}
          maxMove={move}
        />
      </div>

      {/* НИЖНИЙ ПРЯМОУГОЛЬНИК — ещё левее и слегка выше */}
      <div
        className="absolute"
        style={{ left: "50%", top: "50%", transform: "translate(-270px,-20px)" }}
      >
        <ShapeWithEyes
          w={320}
          h={180}
          rotate={8}
          gradient="from-violet-500/60 to-fuchsia-400/40"
          eyeSize={48}
          aperture={baseAperture}
          maxMove={move}
        />
      </div>
    </div>
  );
}

