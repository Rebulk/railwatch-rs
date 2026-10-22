import react from "@vitejs/plugin-react"
import { defineConfig } from "vitest/config"

const frontendDir = new URL("./app/frontend", import.meta.url).pathname

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
