import react from "@vitejs/plugin-react";
import {defineConfig} from "vite";

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/v1": "http://127.0.0.1:8080",
      "/health_check": "http://127.0.0.1:8080",
      "/api-docs": "http://127.0.0.1:8080",
    },
  },
});
