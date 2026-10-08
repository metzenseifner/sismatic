import react from "@vitejs/plugin-react";
import {defineConfig} from "vite";

// The server the dev proxy forwards the API to. 9000 is what the shipped
// `crates/sismatic-server/server_configuration.yaml` binds and what `pnpm
// gen:api` reads the schema from — the Rust default of 8080 only applies when
// no config file is given. Override for a server started elsewhere:
//
//   SISMATIC_SERVER=http://127.0.0.1:8080 pnpm dev
const target = process.env.SISMATIC_SERVER ?? "http://127.0.0.1:9000";

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/v1": target,
      "/health_check": target,
      "/api-docs": target,
    },
  },
});
