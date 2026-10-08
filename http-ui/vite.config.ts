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
  // The server embeds this build and serves it from one subtree rather than
  // from `/`, so that the API's deliberate 404s and 405s stay 404s and 405s —
  // a single-page app served from the root answers its own index.html to every
  // unrouted path, which would quietly turn `/health` into a 200. See
  // `sismatic_http_api::ui`. This is the half of that decision vite owns: the
  // asset URLs it writes into index.html have to be absolute under the same
  // subtree. `pnpm dev` then serves the app at http://localhost:5173/ui/.
  base: "/ui/",
  plugins: [react()],
  server: {
    proxy: {
      "/v1": target,
      "/health_check": target,
      "/api-docs": target,
    },
  },
});
