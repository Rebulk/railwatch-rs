import { fireEvent, render, screen } from "@testing-library/react"
import type { ReactNode } from "react"
import { describe, expect, it, vi } from "vitest"

import Visits from "@/pages/visits/index"
import * as R from "@/routes"

const router = vi.hoisted(() => ({ visit: vi.fn() }))
vi.mock("@inertiajs/react", () => ({
  router,
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
vi.mock("@/components/railwatch/chart-panel", () => ({
  VolumePanel: () => null,
  DurationPanel: () => null,
}))

describe("Visits components table", () => {
  it("keeps the component filter when sorting", () => {
    render(
      <Visits
        components={[]}
        series={[]}
        recent={[]}
        vitals={{}}
        sort="count"
        dir="desc"
        q="component:Widgets/Show"
      />,
    )
    fireEvent.click(screen.getByRole("button", { name: /Avg/ }))
    expect(router.visit).toHaveBeenCalledExactlyOnceWith(
      R.applicationEnvironmentVisitsPath(1, 2, {
        window: "24h",
        q: "component:Widgets/Show",
        sort: "avg",
        dir: "desc",
      }),
      { preserveState: true },
    )
  })
})

const visit = {
  id: 1,
  component: "auth/sign_in",
  url: "/sign_in",
  method: "GET",
  duration: 1500,
  status: "success",
  partial: false,
  only: [],
  props_bytes: 100,
  occurred_at: "2026-10-25T12:00:00.000Z",
  user_ref: null,
  lcp: null,
  cls: null,
  inp: null,
  ttfb: 1300,
  redirect_ms: 1180,
  redirect_count: null,
  connect_ms: 20,
  server_ms: 100,
}

describe("Visits TTFB breakdown", () => {
  it("shows where the initial loads' TTFB went, with redirects as a floor", () => {
    render(
      <Visits
        components={[]}
        series={[]}
        recent={[]}
        vitals={{}}
        ttfb_phases={{
          samples: 5,
          redirect_ms: 1180,
          connect_ms: 40,
          server_ms: 118,
          redirected_at_least: 0.2,
        }}
        sort="count"
        dir="desc"
        q=""
      />,
    )
    expect(screen.getByText("TTFB: redirects")).toBeTruthy()
    expect(screen.getByText("1.18s")).toBeTruthy()
    expect(screen.getByText("118ms")).toBeTruthy()
    expect(screen.getByText("≥ 20.0%")).toBeTruthy()
  })

  it("leaves the breakdown out when no load sent it", () => {
    render(
      <Visits
        components={[]}
        series={[]}
        recent={[]}
        vitals={{}}
        sort="count"
        dir="desc"
        q=""
      />,
    )
    expect(screen.queryByText("TTFB: redirects")).toBeNull()
  })

  it("shows a load's redirect time without claiming a count the browser hid", () => {
    const { rerender } = render(
      <Visits
        components={[]}
        series={[]}
        recent={[visit]}
        vitals={{}}
        sort="count"
        dir="desc"
        q=""
      />,
    )
    expect(screen.getByText("TTFB 1.30s · redirect 1.18s")).toBeTruthy()

    rerender(
      <Visits
        components={[]}
        series={[]}
        recent={[{ ...visit, redirect_count: 2 }]}
        vitals={{}}
        sort="count"
        dir="desc"
        q=""
      />,
    )
    expect(screen.getByText("TTFB 1.30s · redirect 1.18s (2)")).toBeTruthy()
  })
})
