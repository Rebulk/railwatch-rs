import { describe, expect, it } from "vitest"

import { foldRuns, type JourneyEvent } from "./journey"

const ev = (at: number, lane: number | null, run: number): JourneyEvent => ({
  at: new Date(Date.UTC(2026, 9, 28, 12, 0, at)).toISOString(),
  type: "request",
  lane,
  run,
  error: false,
  name: `r${run}`,
})

describe("foldRuns", () => {
  // t1 polls (run 1) while t2 opens a page (runs 2, 3).
  const events = [
    ev(0, 0, 0),
    ev(5, 0, 1),
    ev(12, 1, 2),
    ev(13, 1, 3),
    ev(15, 0, 1),
    ev(25, 0, 1),
  ]

  it("folds a run that another tab interleaves with into one row at its first event", () => {
    const rows = foldRuns(events, {})
    expect(rows.map((r) => [r.run, r.events.length])).toEqual([
      [0, 1],
      [1, 3],
      [2, 1],
      [3, 1],
    ])
  })

  it("loses no event when folded or opened", () => {
    const total = (open: Record<number, boolean>) =>
      foldRuns(events, open).reduce((n, r) => n + r.events.length, 0)
    expect(total({})).toBe(events.length)
    expect(total({ 1: true })).toBe(events.length)
  })

  it("numbers an opened run's rows in order and knows the run's size", () => {
    const rows = foldRuns(events, { 1: true }).filter((r) => r.run === 1)
    expect(rows.map((r) => [r.index, r.size])).toEqual([
      [0, 3],
      [1, 3],
      [2, 3],
    ])
  })

  it("lists an opened run's events in time order among the others", () => {
    const rows = foldRuns(events, { 1: true })
    expect(rows.map((r) => r.events[0].at.slice(17, 19))).toEqual([
      "00",
      "05",
      "12",
      "13",
      "15",
      "25",
    ])
  })
})
