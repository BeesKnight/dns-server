import React, { useEffect, useState } from "react";
import { BrowserRouter, Routes, Route, Navigate } from "react-router-dom";
import AuthPage from "./pages/Auth";
import Home from "./pages/Home";
import Services from "./pages/Services"; // <- добавили
import Agents from "./pages/Agents";
import { useAuth } from "./store/auth";

/** Защищённый маршрут: ждём bootstrap(), потом пускаем только если user есть */
function Protected({ children }: { children: React.ReactElement }) {
  const { user, bootstrap } = useAuth();
  const [booted, setBooted] = useState(false);

  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        await bootstrap();
      } finally {
        if (alive) setBooted(true);
      }
    })();
    return () => {
      alive = false;
    };
  }, [bootstrap]);

  if (!booted) return null; // тут можно показать сплэш/скелетон
  if (!user) return <Navigate to="/" replace />; // не авторизован → логин
  return children;
}

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        {/* старт — логин/регистрация */}
        <Route path="/" element={<AuthPage />} />

        {/* приложение — только после входа */}
        <Route
          path="/app"
          element={
            <Protected>
              <Home />
            </Protected>
          }
        />
        <Route
          path="/app/services"
          element={
            <Protected>
              <Services />
            </Protected>
          }
        />
        <Route
          path="/app/agents"
          element={
            <Protected>
              <Agents />
            </Protected>
          }
        />

        {/* fallback */}
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </BrowserRouter>
  );
}
