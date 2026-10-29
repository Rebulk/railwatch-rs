import { fireEvent, render, screen } from "@testing-library/react"
import type { ReactNode } from "react"
import { describe, expect, it, vi } from "vitest"

import type { JourneyEvent } from "@/lib/journey"
import { ms } from "@/lib/format"
import BrowserSessionShow, { runDuration } from "@/pages/browser_sessions/show"

vi.mock("@inertiajs/react", () => ({
  Link: ({
    href,
    children,
    className,
  }: {
    href: string
    children: ReactNode
    className?: string
  }) => (
    <a href={href} className={className}>
      {children}
    </a>
  ),
  usePage: () => ({
    props: { environment: { application_id: 1, id: 2 }, window: "24h" },
  }),
}))
vi.mock("@/layouts/env-layout", () => ({
  default: ({ children }: { children: ReactNode }) => <>{children}</>,
}))
vi.mock("@/components/railwatch/page-header", () => ({
  PageHeader: () => null,
}))

const at = (s: number) =>
  new Date(Date.UTC(2026, 9, 28, 12, 0, s)).toISOString()
const req = (
  s: number,
  lane: number | null,
  run: number,
  name: string,
  status = 200,
): JourneyEvent => ({
  at: at(s),
  type: "request",
  lane,
  run,
  error: status >= 500,
  name,
  status,
  duration: 2,
  execution_id: `e${s}`,
})

const tabs = [
  {
    id: "tab-A",
    label: "t1",
    first: at(0),
    last: at(25),
    requests: 4,
    visits: 1,
    errors: 0,
  },
  {
    id: "tab-B",
    label: "t2",
    first: at(12),
    last: at(13),
    requests: 1,
    visits: 1,
    errors: 1,
  },
]
const events: JourneyEvent[] = [
  {
    at: at(0),
    type: "visit",
    lane: 0,
    run: 0,
    error: false,
    component: "orders/index",
    url: "/orders",
    duration: 120,
  },
  req(1, 0, 1, "GET /orders"),
  req(5, 0, 2, "GET /api/poll"),
  {
    at: at(12),
    type: "visit",
    lane: 1,
    run: 3,
    error: false,
    component: "orders/show",
    url: "/orders/7",
    duration: 0,
  },
  req(13, 1, 4, "GET /orders/7", 500),
  req(15, 0, 2, "GET /api/poll"),
  req(25, 0, 2, "GET /api/poll"),
  req(30, null, 5, "GET /settings"),
]

const props = {
  session_id: "s1",
  started: at(0),
  last: at(30),
  tabs,
  events,
  runs: 6,
  users: ["ada"],
  ips: [],
  user_agents: [],
  capped: false,
  cap: 20000,
}

// The row of the event labelled `text`, and which lane cells it marks.
function lanesOf(text: string): boolean[] {
  const row = screen.getByText(text).closest("a, div[class*='grid']")!
  const cells = row.querySelectorAll("[aria-hidden] > span.rounded-sm")
  return Array.from(cells).map((c) => !c.className.includes("bg-muted"))
}

describe("runDuration", () => {
  const d = (...xs: number[]) =>
    xs.map((x, i) => ({ ...req(i, 0, 0, "x"), duration: x }))
  it("is the median and the max of the timed events", () => {
    expect(runDuration(d(1, 9, 3))).toBe(`${ms(3)} · max ${ms(9)}`)
    expect(runDuration(d(2, 2))).toBe(ms(2))
    expect(runDuration(d(0, 0))).toBe("")
  })
})

describe("Session journey lanes", () => {
  it("marks each event in its own tab's lane, polls folded into one row", () => {
    render(<BrowserSessionShow {...props} />)
    expect(lanesOf("orders/index /orders")).toEqual([true, false])
    expect(lanesOf("orders/show /orders/7")).toEqual([false, true])
    expect(lanesOf("GET /orders/7")).toEqual([false, true])
    expect(lanesOf("GET /settings")).toEqual([false, false])
    expect(screen.getAllByText("GET /api/poll")).toHaveLength(1)
    expect(screen.getByText(/×3 over 20s/)).toBeTruthy()
    // The folded polls show their duration (all 2 ms); a visit without one shows none.
    const polls = screen
      .getByText("GET /api/poll")
      .closest("div[class*='grid']")!
    expect(polls.textContent).toContain("2.0ms")
    const untimed = screen
      .getByText("orders/show /orders/7")
      .closest("a, div[class*='grid']")!
    expect(untimed.textContent).not.toContain("ms")
    expect(screen.getByText("no tab")).toBeTruthy()
  })

  it("opens a folded run into its events, in time order with the other tab's", () => {
    render(<BrowserSessionShow {...props} />)
    fireEvent.click(screen.getByRole("button", { expanded: false }))
    const rows = screen.getAllByText(/^(GET|orders)/).map((n) => n.textContent)
    expect(rows).toEqual([
      "orders/index /orders",
      "GET /orders",
      "GET /api/poll",
      "orders/show /orders/7",
      "GET /orders/7",
      "GET /api/poll",
      "GET /api/poll",
      "GET /settings",
    ])
  })
})
