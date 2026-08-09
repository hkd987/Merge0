import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Dev loop: `npm run dev` serves the SPA on :5173 and proxies API calls to
// a locally running merge0-server (MERGE0_DEV_FAKES=1 recommended).
// Production: `npm run build` emits ui/dist, which merge0-server embeds.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/reports": "http://127.0.0.1:8080",
      "/telemetry": "http://127.0.0.1:8080",
      "/onboarding": "http://127.0.0.1:8080",
      "/safety": "http://127.0.0.1:8080",
      "/healthz": "http://127.0.0.1:8080",
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/__tests__/setup.ts"],
    globals: true,
  },
});
