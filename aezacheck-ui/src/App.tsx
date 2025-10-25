import React, { useEffect, useState } from "react";
import { BrowserRouter, Routes, Route, Navigate } from "react-router-dom";
import AuthPage from "./pages/Auth";
import Home from "./pages/Home";
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

  if (!booted) return null;                // можно отрендерить сплэш
  if (!user) return <Navigate to="/" replace />; // не авторизован → на логин
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
        {/* fallback */}
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </BrowserRouter>
  );
}
