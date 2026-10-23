import { act, cleanup, renderHook } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import { initializeTheme, useAppearance } from "@/hooks/use-appearance"

// jsdom has no matchMedia. This one lets a test flip the OS theme and
// dispatches "change" to whatever listeners are registered at the time.
let dark = false
let listeners: Set<() => void>

function flipSystemTheme(toDark: boolean) {
  dark = toDark
  for (const listener of [...listeners]) listener()
}

const isDark = () => document.documentElement.classList.contains("dark")

beforeEach(() => {
  dark = false
  listeners = new Set()
  localStorage.clear()
  vi.stubGlobal("matchMedia", (query: string) => ({
    get matches() {
      return dark
    },
    media: query,
    addEventListener: (_: string, l: () => void) => listeners.add(l),
    removeEventListener: (_: string, l: () => void) => listeners.delete(l),
  }))
  initializeTheme()
})

afterEach(() => {
  // Unmount while the matchMedia stub is still in place.
  cleanup()
  vi.unstubAllGlobals()
  document.documentElement.classList.remove("dark")
})

describe("useAppearance", () => {
  it("keeps following the OS theme after the settings page is left", () => {
    const { unmount } = renderHook(() => useAppearance())
    unmount()

    flipSystemTheme(true)

    expect(isDark()).toBe(true)
  })

  it("follows the OS theme again after switching away from system and back", () => {
    const { result } = renderHook(() => useAppearance())
    act(() => result.current.updateAppearance("light"))
    act(() => result.current.updateAppearance("system"))

    flipSystemTheme(true)

    expect(isDark()).toBe(true)
  })

  it("ignores the OS theme while an explicit appearance is chosen", () => {
    const { result } = renderHook(() => useAppearance())
    act(() => result.current.updateAppearance("light"))

    flipSystemTheme(true)

    expect(isDark()).toBe(false)
  })
})
