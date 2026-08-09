import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The build output is embedded into `ancre-control` by rust-embed, so `dist`
// has to stay where `#[folder = "ui/dist"]` expects it.
//
// `base: "./"` matters: the dashboard is served from the same origin as the
// API, and relative asset URLs keep it working behind a path prefix if someone
// puts it under a reverse proxy.
export default defineConfig({
  base: "./",
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": new URL("./src", import.meta.url).pathname } },
  build: { outDir: "dist", emptyOutDir: true, sourcemap: false },
  server: {
    port: 5173,
    // `npm run dev` talks to a control plane on 8081. Same paths as
    // production, so nothing in the app knows which mode it is in.
    proxy: {
      "/v1": "http://localhost:8081",
      "/api": "http://localhost:8081",
      "/healthz": "http://localhost:8081",
    },
  },
});
