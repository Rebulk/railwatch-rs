// railwatch-rs builds the same dashboard railwatch-cloud does (two roots, see
// frontend.config.ts), but with plain Vite: no rails-vite-plugin, no SSR. The
// Rust server embeds dist/ and serves URL /vite/<file> from dist/<file>, with
// dist/.vite/manifest.json mapping each entrypoint to its hashed file.
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

import { gemFrontend, twoRoots } from "./frontend.config"

export default defineConfig({
  // Relative, so chunk imports and CSS font URLs resolve against the file
  // that names them: the same build works at / and under a base path
  // (RAILWATCH_BASE_PATH). The server still serves dist/ at /vite/.
  base: "./",
  // icon.png and icon.svg land at dist/ root.
  publicDir: "tmp/railwatch-frontend/public",
  server: { fs: { allow: [".", gemFrontend] } },
  resolve: {
    alias: [
      // routes/index.js is the gem's (cloud's copy is identical). twoRoots()
      // only tries index.ts/index.tsx, and the rails-vite-plugin `@` alias
      // that found cloud's copy is not used here.
      { find: /^@\/routes$/, replacement: `${gemFrontend}/routes/index.js` },
    ],
  },
  build: {
    outDir: "dist",
    assetsDir: "assets",
    emptyOutDir: true,
    manifest: true,
    rollupOptions: {
      input: [
        "app/frontend/entrypoints/inertia.tsx",
        "app/frontend/entrypoints/application.css",
      ],
    },
  },
  plugins: [
    twoRoots(),
    react({
      babel: {
        plugins: ["babel-plugin-react-compiler"],
      },
    }),
    tailwindcss(),
  ],
})
