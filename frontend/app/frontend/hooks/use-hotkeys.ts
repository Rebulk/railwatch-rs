import { useEffect, useRef } from "react"

import { ignorePageShortcut } from "@/lib/keyboard"

type HotkeyMap = Record<string, () => void>

const CHORD_TIMEOUT_MS = 800

// Tiny keyboard shortcut hook supporting single keys ("?") and two-key
// chords ("g d") without pulling in a dependency. Yields to the same
// keystrokes every page shortcut does (ignorePageShortcut), except Shift,
// which "?" needs.
export function useHotkeys(map: HotkeyMap) {
  const mapRef = useRef(map)
  useEffect(() => {
    mapRef.current = map
  })

  const pending = useRef<string | null>(null)
  const timer = useRef<number | undefined>(undefined)

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (ignorePageShortcut(event, { allowShift: true })) return

      if (pending.current) {
        const combo = `${pending.current} ${event.key}`
        window.clearTimeout(timer.current)
        pending.current = null
        if (mapRef.current[combo]) {
          event.preventDefault()
          mapRef.current[combo]()
          return
        }
      }

      if (mapRef.current[event.key]) {
        event.preventDefault()
        mapRef.current[event.key]()
        return
      }

      if (event.key === "g") {
        pending.current = "g"
        timer.current = window.setTimeout(() => {
          pending.current = null
        }, CHORD_TIMEOUT_MS)
      }
    }

    document.addEventListener("keydown", onKeyDown)
    return () => {
      document.removeEventListener("keydown", onKeyDown)
      window.clearTimeout(timer.current)
    }
  }, [])
}
