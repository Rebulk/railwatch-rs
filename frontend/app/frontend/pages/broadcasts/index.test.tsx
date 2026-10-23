import { render, screen } from "@testing-library/react"
import type { ReactNode } from "react"
import { describe, expect, it, vi } from "vitest"

import Broadcasts from "@/pages/broadcasts/index"

vi.mock("@inertiajs/react", () => ({
  router: { visit: vi.fn() },
  usePage: () => ({
    props: { environment: { application_id: 1, id: 2 }, window: "24h" },
  }),
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

const broadcast = (id: number, source: string, preview: string) => ({
  id,
  kind: "broadcast",
  failed: false,
  stream: "widgets",
  channel: null,
  action: null,
  bytes: 10,
  duration: 1,
  occurred_at: "2026-10-22T10:00:00Z",
  execution_id: `exec-${id}`,
  execution_source: source,
  execution_preview: preview,
})

describe("recent broadcasts", () => {
  it("link to the execution page that matches what sent them", () => {
    render(
      <Broadcasts
        streams={[]}
        recent={[
          broadcast(1, "request", "POST /widgets"),
          broadcast(2, "job", "WidgetJob"),
          broadcast(3, "channel_action", "WidgetsChannel#speak"),
        ]}
      />,
    )
    expect(
      screen.getByRole("link", { name: "POST /widgets" }),
    ).toHaveAttribute("href", "/apps/1/envs/2/requests/exec-1")
    expect(screen.getByRole("link", { name: "WidgetJob" })).toHaveAttribute(
      "href",
      "/apps/1/envs/2/jobs/exec-2",
    )
    expect(
      screen.getByRole("link", { name: "WidgetsChannel#speak" }),
    ).toHaveAttribute("href", "/apps/1/envs/2/executions/exec-3")
  })
})
