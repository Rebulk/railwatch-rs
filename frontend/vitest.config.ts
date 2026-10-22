import { fileURLToPath } from "node:url"

import react from "@vitejs/plugin-react"
import { defineConfig } from "vitest/config"

const frontendDir = fileURLToPath(new URL("./app/frontend", import.meta.url))

// Formatted times in the tests are UTC; the workers inherit this.
process.env.TZ = "UTC"

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": frontendDir,
      "~": frontendDir,
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./app/frontend/test-setup.ts"],
    include: ["app/frontend/**/*.test.{ts,tsx}"],
  },
})
