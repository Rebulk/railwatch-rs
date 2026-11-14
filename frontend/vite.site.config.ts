// railwatch.dev: the landing page and the hosted service's legal pages as a
// static site (site/), sharing the app's components and stylesheet.
// site/build.sh runs it; Cloudflare Pages serves it. @inertiajs/react and
// @/routes are swapped for site/'s stand-ins: a static page has no Inertia
// server, and its app links go to VITE_APP_URL.
import { fileURLToPath } from "node:url"

import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

const here = (p: string) => fileURLToPath(new URL(p, import.meta.url))

export default defineConfig({
  root: here("./site"),
  publicDir: here("./site/public"),
  resolve: {
    alias: [
      { find: "@inertiajs/react", replacement: here("./site/inertia-shim.tsx") },
      { find: /^@\/routes$/, replacement: here("./site/routes.ts") },
      { find: "@", replacement: here("./app/frontend") },
    ],
  },
  build: {
    outDir: here("./dist-site"),
    emptyOutDir: true,
    rollupOptions: {
      input: {
        home: here("./site/index.html"),
        terms: here("./site/legal/terms/index.html"),
        privacy: here("./site/legal/privacy/index.html"),
      },
    },
  },
  plugins: [
    react({ babel: { plugins: ["babel-plugin-react-compiler"] } }),
    tailwindcss(),
  ],
})
