import { fireEvent, render, screen } from "@testing-library/react"
import type { ReactNode } from "react"
import { beforeEach, describe, expect, it, vi } from "vitest"

import AlertsIndex from "@/pages/alerts/index"
import * as R from "@/routes"

const router = vi.hoisted(() => ({ visit: vi.fn(), post: vi.fn() }))
vi.mock("@inertiajs/react", () => ({
  router,
  usePage: () => ({ props: { environment: null } }),
  Link: ({
    href,
    className,
    children,
  }: {
    href: string
    className?: string
    children: ReactNode
  }) => (
    <a href={href} className={className}>
      {children}
    </a>
  ),
}))
vi.mock("@/layouts/env-layout", () => ({
  default: ({ children }: { children: ReactNode }) => <>{children}</>,
}))
vi.mock("@/components/railwatch/page-header", () => ({
  PageHeader: () => null,
}))

const failedAlert = {
  id: 3,
  event: "new_issue",
  status: "failed",
  sent_at: null,
  error: "Slack said no",
  payload: { text: "boom" },
  created_at: "2026-10-22T10:00:00Z",
  integration: { kind: "slack", name: "#alerts" },
  issue: { id: 9, key: "APP-9", title: "Boom" },
}

beforeEach(() => vi.clearAllMocks())

function renderAlerts() {
  render(<AlertsIndex alerts={[failedAlert]} q="" statuses={["failed"]} />)
  return screen.getByRole("button", { name: /new_issue/ })
}

describe("alert rows", () => {
  it("keeps the issue link and Retry out of the expand toggle", () => {
    const toggle = renderAlerts()
    expect(toggle.querySelector("a, button")).toBeNull()
    expect(screen.getByRole("link", { name: /APP-9/ })).toHaveAttribute(
      "href",
      R.issuePath(9),
    )
  })

  it("expands the row from the toggle and reports it", () => {
    const toggle = renderAlerts()
    expect(toggle).toHaveAttribute("aria-expanded", "false")
    fireEvent.click(toggle)
    expect(toggle).toHaveAttribute("aria-expanded", "true")
    expect(screen.getByText("Slack said no")).toBeInTheDocument()
  })

  it("retries without expanding the row", () => {
    const toggle = renderAlerts()
    fireEvent.click(screen.getByRole("button", { name: "Retry" }))
    expect(router.post).toHaveBeenCalledExactlyOnceWith(
      R.retryAlertPath(3),
      {},
      { preserveScroll: true },
    )
    expect(toggle).toHaveAttribute("aria-expanded", "false")
  })
})
