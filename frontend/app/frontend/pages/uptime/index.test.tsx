import { render, screen } from "@testing-library/react"
import type { ReactNode } from "react"
import { describe, expect, it, vi } from "vitest"

import Uptime, { type Day, type Monitor } from "@/pages/uptime/index"

vi.mock("@inertiajs/react", () => ({
  router: { patch: vi.fn(), delete: vi.fn(), get: vi.fn() },
  usePage: () => ({
    props: { environment: { id: 2, application_id: 1, name: "production" } },
  }),
  Form: ({
    children,
  }: {
    children: (a: { errors: object; processing: boolean }) => ReactNode
  }) => <form>{children({ errors: {}, processing: false })}</form>,
}))
vi.mock("@/layouts/env-layout", () => ({
  default: ({ children }: { children: ReactNode }) => <>{children}</>,
}))
vi.mock("@/components/railwatch/page-header", () => ({
  PageHeader: () => null,
}))
// Recharts needs a measured container; the chart is not under test here.
vi.mock("@/components/railwatch/metric-chart", () => ({
  MetricChart: () => <div data-testid="chart" />,
}))

function days(): Day[] {
  return Array.from({ length: 90 }, (_, i) => ({
    date: `d${i}`,
    uptime: i === 89 ? 50 : i === 88 ? 99.5 : i < 10 ? null : 100,
    checks: i < 10 ? 0 : 100,
    failures: 0,
  }))
}

function monitor(over: Partial<Monitor> = {}): Monitor {
  return {
    id: 7,
    name: "Storefront",
    url: "https://shop.example.com/up",
    method: "GET",
    expected_status: null,
    keyword: null,
    timeout_seconds: 10,
    interval_seconds: 60,
    enabled: true,
    public: false,
    public_name: null,
    public_url: false,
    status: "down",
    status_since: "2026-11-01T10:00:00Z",
    last_checked_at: "2026-11-01T10:05:00Z",
    last_status_code: 503,
    last_latency_ms: 40,
    last_error: "status 503, expected 2xx or 3xx",
    uptime: { "24h": 99.95, "7d": 98.5, "30d": null, "90d": 100 },
    days: days(),
    latency: [{ t: "2026-11-01T10:00:00Z", value: 40 }],
    ...over,
  }
}

const limits = {
  min_interval_seconds: 30,
  max_interval_seconds: 3600,
  max_timeout_seconds: 30,
  max_monitors: 20,
  failures_to_down: 2,
}

describe("Uptime page", () => {
  it("shows each monitor's status, uptime windows, 90 day bars and the error", () => {
    render(
      <Uptime
        monitors={[monitor()]}
        incidents={[]}
        status_page={null}
        limits={limits}
        can_manage={false}
        window="24h"
      />,
    )
    expect(screen.getByText("Storefront")).toBeInTheDocument()
    expect(screen.getAllByText("Down").length).toBeGreaterThan(0)
    expect(screen.getByText("99.95%")).toBeInTheDocument()
    expect(screen.getByText("98.50%")).toBeInTheDocument()
    expect(screen.getByText("status 503, expected 2xx or 3xx")).toBeInTheDocument()
    const bars = screen.getByRole("img", { name: /last 90 days/ })
    expect(bars.children).toHaveLength(90)
    const tones = Array.from(bars.children).map((b) =>
      b.getAttribute("data-tone"),
    )
    expect(tones.slice(-3)).toEqual(["up", "partial", "down"])
    expect(tones[0]).toBe("none")
    // Members see no management controls.
    expect(screen.queryByText("Add a monitor")).toBeNull()
    expect(screen.queryByRole("button", { name: "Delete" })).toBeNull()
  })

  it("gives admins the form and the status page settings", () => {
    render(
      <Uptime
        monitors={[]}
        incidents={[]}
        status_page={{
          slug: "acme",
          title: "Acme",
          enabled: true,
          url: "http://x/status/acme",
        }}
        limits={limits}
        can_manage
        window="24h"
      />,
    )
    expect(screen.getByText("Add a monitor")).toBeInTheDocument()
    expect(screen.getByRole("link", { name: "http://x/status/acme" })).toBeInTheDocument()
    // The hidden "off" precedes the checkbox so a checked box wins.
    const names = Array.from(
      document.querySelectorAll("input[name='public'], button[name='public']"),
    ).map((n) => n.tagName)
    expect(names[0]).toBe("INPUT")
  })
})
