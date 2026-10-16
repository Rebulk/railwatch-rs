import type { ResolvedComponent } from "@inertiajs/react"

// Inertia pages come from two roots: this app's own (hosting: marketing,
// sign-in, accounts, settings, usage, docs) and the railwatch gem's (every
// dashboard page), linked at tmp/railwatch-frontend. This app's page wins
// when both have one. Vite resolves the globs at build time; the paths are
// project-root absolute.
const lazy = import.meta.glob<{ default: ResolvedComponent }>([
  "/app/frontend/pages/**/*.tsx",
  "/tmp/railwatch-frontend/pages/**/*.tsx",
  "!**/*.test.tsx",
])

export function findPage<T>(name: string, pages: Record<string, T>): T {
  const page =
    pages[`/app/frontend/pages/${name}.tsx`] ??
    pages[`/tmp/railwatch-frontend/pages/${name}.tsx`]
  if (!page) throw new Error(`Missing Inertia page component: '${name}.tsx'`)
  return page
}

export function loadPage(name: string) {
  return findPage(name, lazy)()
}
