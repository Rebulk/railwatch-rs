import { render, screen } from "@testing-library/react"
import type { ReactNode } from "react"
import { describe, expect, it, vi } from "vitest"

import Logs from "@/pages/logs/index"

vi.mock("@inertiajs/react", () => ({
  router: { visit: vi.fn() },
  usePage: () => ({
    url: "/apps/1/envs/2/logs",
    props: {
      environment: { application_id: 1, id: 2 },
      window: "24h",
      saved_views: [],
    },
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

const log = (id: number, source: string, preview: string) => ({
  id,
  level: "info",
  message: `line ${id}`,
  tags: [],
  occurred_at: "2026-10-22T10:00:00Z",
  execution_id: `exec-${id}`,
  execution_source: source,
  execution_preview: preview,
  source: null,
  tenant: null,
  user_ref: null,
  context: null,
})

describe("log rows", () => {
  it("link to the execution page that matches what wrote the line", () => {
    render(
      <Logs
        logs={[
          log(1, "request", "GET /widgets"),
          log(2, "job", "WidgetJob"),
          log(3, "command", "rails db:migrate"),
        ]}
        counts={{}}
        highlights={{}}
        pagination={{ limit: 50, next_cursor: null, has_more: false }}
        q=""
      />,
    )
    expect(screen.getByRole("link", { name: "GET /widgets" })).toHaveAttribute(
      "href",
      "/apps/1/envs/2/requests/exec-1",
    )
    expect(screen.getByRole("link", { name: "WidgetJob" })).toHaveAttribute(
      "href",
      "/apps/1/envs/2/jobs/exec-2",
    )
    expect(
      screen.getByRole("link", { name: "rails db:migrate" }),
    ).toHaveAttribute("href", "/apps/1/envs/2/commands/exec-3")
  })
})
