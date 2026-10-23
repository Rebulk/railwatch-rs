import { fireEvent, render, screen } from "@testing-library/react"
import type { ReactNode } from "react"
import { beforeEach, describe, expect, it, vi } from "vitest"

import Requests from "@/pages/requests/index"
import * as R from "@/routes"
import type { GroupRow } from "@/types"

const router = vi.hoisted(() => ({ visit: vi.fn() }))
let shared: Record<string, unknown> = {}
vi.mock("@inertiajs/react", () => ({
  router,
  usePage: () => ({ url: "/apps/1/envs/2/requests", props: shared }),
  Link: ({ href, children }: { href: string; children: ReactNode }) => (
    <a href={href}>{children}</a>
  ),
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

const from = "2026-10-20T00:00:00.000000Z"
const to = "2026-10-21T00:00:00.000000Z"
const route: GroupRow = {
  group_hash: "abc",
  name: "GET /widgets",
  count: 1,
  errors: 0,
  client_errors: 0,
  avg: 1,
  p50: 1,
  p95: 1,
  p99: 1,
  max: 1,
  sparkline: [1],
}

beforeEach(() => {
  vi.clearAllMocks()
  shared = {
    environment: { application_id: 1, id: 2 },
    window: "custom",
    range: { from, to },
  }
})

function renderRequests() {
  render(
    <Requests
      routes={[route]}
      series={[]}
      deploys={[]}
      sort="count"
      dir="desc"
      q="status:5xx"
    />,
  )
}

describe("Requests with a custom range", () => {
  it("opens a route within the same range", () => {
    renderRequests()
    expect(screen.getByRole("link", { name: "GET /widgets" })).toHaveAttribute(
      "href",
      R.routeApplicationEnvironmentRequestsPath(1, 2, "abc", { from, to }),
    )
  })

  it("keeps the range when filtering", () => {
    renderRequests()
    const input = screen.getByPlaceholderText(
      "method:GET status:5xx route:/widgets",
    )
    fireEvent.change(input, { target: { value: "status:4xx" } })
    fireEvent.keyDown(input, { key: "Enter" })
    expect(router.visit).toHaveBeenCalledExactlyOnceWith(
      R.applicationEnvironmentRequestsPath(1, 2, {
        from,
        to,
        sort: "count",
        dir: "desc",
        q: "status:4xx",
      }),
      { preserveState: true },
    )
  })

  it("still sends a preset window as window", () => {
    shared = { ...shared, window: "7d", range: { from, to } }
    renderRequests()
    expect(screen.getByRole("link", { name: "GET /widgets" })).toHaveAttribute(
      "href",
      R.routeApplicationEnvironmentRequestsPath(1, 2, "abc", { window: "7d" }),
    )
  })
})
