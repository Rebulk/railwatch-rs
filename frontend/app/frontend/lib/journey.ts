// A session journey's events (the server's browser_sessions/show props),
// and the timeline rows the page shows: each run of alike events in one
// tab (the server's `run`) folds into one row unless it is open.

export interface JourneyEvent {
  at: string
  type: "request" | "visit" | "exception" | "browser_error"
  lane: number | null
  run: number
  error: boolean
  duration?: number
  // request
  execution_id?: string
  name?: string
  path?: string
  status?: number
  outcome?: string
  exception?: string
  // visit
  component?: string
  url?: string
  method?: string
  partial?: boolean
  lcp?: number
  inp?: number
  ttfb?: number
  cls?: number
  // exception / browser error
  class_name?: string
  message?: string
  group_hash?: string
  browser?: boolean
}

export interface Row {
  run: number
  // The row's place among its run's rows (0 for a folded run).
  index: number
  // Events in the whole run (1: a run of one, nothing to fold).
  size: number
  events: JourneyEvent[]
}

// Events in time order; a run's events need not be adjacent (another tab
// can act in between), so a folded run sits at its first event.
export function foldRuns(
  events: JourneyEvent[],
  open: Record<number, boolean>,
): Row[] {
  const byRun = new Map<number, JourneyEvent[]>()
  for (const e of events) {
    const list = byRun.get(e.run)
    if (list) list.push(e)
    else byRun.set(e.run, [e])
  }
  const rows: Row[] = []
  const seen = new Set<number>()
  // An opened run's next row index: byRun lists each run in `events` order.
  const next = new Map<number, number>()
  for (const e of events) {
    const all = byRun.get(e.run)!
    if (open[e.run] || all.length === 1) {
      const index = next.get(e.run) ?? 0
      next.set(e.run, index + 1)
      rows.push({ run: e.run, index, size: all.length, events: [e] })
    } else if (!seen.has(e.run)) {
      seen.add(e.run)
      rows.push({ run: e.run, index: 0, size: all.length, events: all })
    }
  }
  return rows
}
