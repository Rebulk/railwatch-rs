import { describe, expect, it } from "vitest"

import { dayTone, duration, overall, uptimePct } from "@/lib/uptime"

describe("uptime helpers", () => {
  it("never shows a day with failures as 100%", () => {
    expect(uptimePct(99.999)).toBe("99.99%")
    expect(uptimePct(100)).toBe("100%")
    expect(uptimePct(null)).toBe("–")
    expect(dayTone(99.99)).toBe("partial")
    expect(dayTone(100)).toBe("up")
    expect(dayTone(94.9)).toBe("down")
    expect(dayTone(null)).toBe("none")
  })

  it("reads overall status like the server", () => {
    expect(overall(["up", "up"])).toBe("operational")
    expect(overall(["up", "down"])).toBe("degraded")
    expect(overall(["down"])).toBe("down")
    expect(overall(["pending", "paused"])).toBe("unknown")
  })

  it("writes durations in the units an incident is read in", () => {
    expect(duration(45)).toBe("45 s")
    expect(duration(180)).toBe("3 min")
    expect(duration(3 * 3600 + 5 * 60)).toBe("3 h 5 min")
    expect(duration(2 * 86400 + 4 * 3600)).toBe("2 d 4 h")
  })
})
