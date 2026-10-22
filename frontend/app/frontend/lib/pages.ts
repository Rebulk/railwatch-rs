import type { ResolvedComponent } from "@inertiajs/react"

// Every Inertia page: the dashboard's and hosting's (marketing, sign-in,
// accounts, settings, usage, docs). Vite resolves the glob at build time;
// the paths are project-root absolute.
const lazy = import.meta.glob<{ default: ResolvedComponent }>([
  "/app/frontend/pages/**/*.tsx",
  "!**/*.test.tsx",
])

export function findPage<T>(name: string, pages: Record<string, T>): T {
  const page = pages[`/app/frontend/pages/${name}.tsx`]
  if (!page) throw new Error(`Missing Inertia page component: '${name}.tsx'`)
  return page
}

export function loadPage(name: string) {
  return findPage(name, lazy)()
}
