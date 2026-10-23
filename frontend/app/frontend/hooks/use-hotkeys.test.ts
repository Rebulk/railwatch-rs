import { renderHook } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"

import { useHotkeys } from "@/hooks/use-hotkeys"

function press(
  key: string,
  init: KeyboardEventInit = {},
  target: EventTarget = document.body,
) {
  const event = new KeyboardEvent("keydown", {
    key,
    bubbles: true,
    cancelable: true,
    ...init,
  })
  target.dispatchEvent(event)
  return event
}

function inside(role: string) {
  const el = document.createElement("div")
  el.setAttribute("role", role)
  const item = document.createElement("div")
  el.append(item)
  document.body.append(el)
  return item
}

describe("useHotkeys", () => {
  it("runs ? even though typing it needs Shift", () => {
    const help = vi.fn()
    renderHook(() => useHotkeys({ "?": help }))
    press("?", { shiftKey: true })
    expect(help).toHaveBeenCalledOnce()
  })

  it("runs a g chord", () => {
    const go = vi.fn()
    renderHook(() => useHotkeys({ "g d": go }))
    press("g")
    press("d")
    expect(go).toHaveBeenCalledOnce()
  })

  it("ignores held-down repeats and events a control already handled", () => {
    const help = vi.fn()
    renderHook(() => useHotkeys({ "?": help }))
    press("?", { shiftKey: true, repeat: true })
    const handled = new KeyboardEvent("keydown", {
      key: "?",
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    })
    handled.preventDefault()
    document.body.dispatchEvent(handled)
    expect(help).not.toHaveBeenCalled()
  })

  it("does not start a g chord from inside a menu or listbox", () => {
    const go = vi.fn()
    renderHook(() => useHotkeys({ "g d": go }))
    for (const role of ["menu", "listbox"]) {
      const item = inside(role)
      press("g", {}, item)
      press("d")
      item.parentElement!.remove()
    }
    expect(go).not.toHaveBeenCalled()
  })
})
