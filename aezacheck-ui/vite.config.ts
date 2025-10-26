import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");

  const devPort = Number(env.VITE_DEV_PORT ?? 5173);
  const proxyTarget = env.VITE_API_PROXY_TARGET ?? env.VITE_API_BASE ?? "http://localhost:8088";
  const enableProxy = env.VITE_USE_PROXY === "true" || Boolean(env.VITE_API_PROXY_TARGET);

  return {
    plugins: [react()],
    envPrefix: "VITE_",
    server: {
      port: devPort,
      proxy: enableProxy
        ? {
            "/api": {
              target: proxyTarget,
              changeOrigin: true,
              secure: false,
              rewrite: (path) => path.replace(/^\/api/, ""),
            },
          }
        : undefined,
    },
    define: {
      __APP_VERSION__: JSON.stringify(env.npm_package_version ?? "dev"),
    },
    build: {
      sourcemap: mode !== "production",
    },
    test: {
      globals: true,
      environment: "jsdom",
      setupFiles: "./tests/setup.ts",
      include: ["src/**/*.{test,spec}.{ts,tsx}", "tests/**/*.test.ts"],
      exclude: ["tests/e2e/**"],
      coverage: {
        provider: "v8",
        reporter: ["text", "json-summary", "html"],
      },
    },
  };
});
