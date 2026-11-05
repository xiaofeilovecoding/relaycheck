import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  base: "/",
  build: {
    outDir: "dist",
    assetsInlineLimit: 0,
  },
  server: {
    proxy: {
      "/v1": "http://127.0.0.1:5177",
    },
  },
});
