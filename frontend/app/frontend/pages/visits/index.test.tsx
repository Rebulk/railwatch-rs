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
