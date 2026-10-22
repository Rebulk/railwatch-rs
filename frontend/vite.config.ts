// railwatch-rs builds the dashboard with plain Vite: no rails-vite-plugin, no
// SSR. The Rust server embeds dist/ and serves URL /vite/<file> from
// dist/<file>, with dist/.vite/manifest.json mapping each entrypoint to its
// hashed file.
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

const frontendDir = new URL("./app/frontend", import.meta.url).pathname

export default defineConfig({
  // Relative, so chunk imports and CSS font URLs resolve against the file
  // that names them: the same build works at / and under a base path
  // (RAILWATCH_BASE_PATH). The server still serves dist/ at /vite/.
  base: "./",
  // icon.png and icon.svg land at dist/ root.
  publicDir: "app/frontend/public",
  resolve: {
    alias: {
      "@": frontendDir,
      "~": frontendDir,
    },
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
    react({
      babel: {
        plugins: ["babel-plugin-react-compiler"],
      },
    }),
    tailwindcss(),
  ],
})
