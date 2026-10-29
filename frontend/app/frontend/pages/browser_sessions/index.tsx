import { router, usePage } from "@inertiajs/react"
import { AppWindow } from "lucide-react"

import { DataTable } from "@/components/railwatch/data-table"
import { EmptyState } from "@/components/railwatch/empty-state"
import { FilterBar } from "@/components/railwatch/filter-bar"
import { PageHeader } from "@/components/railwatch/page-header"
import { Segmented, SegmentedItem } from "@/components/railwatch/segmented"
import EnvLayout from "@/layouts/env-layout"
import { ago, count } from "@/lib/format"
import * as R from "@/routes"
import type { SharedProps } from "@/types"

interface BrowserSession {
  session_id: string
  first: string
  last: string
  requests: number
  visits: number
  errors: number
  tabs: number
  users: string[]
}
interface Props {
  sessions: BrowserSession[]
  total: number
  user: string
  errors: boolean
}

function duration(first: string, last: string): string {
  const s = Math.round((Date.parse(last) - Date.parse(first)) / 1000)
  if (s < 120) return `${s}s`
  if (s < 7200) return `${Math.floor(s / 60)}m`
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`
}

// The window's browser sessions (one per browser, shared by its tabs),
// newest activity first. Filter by user, or to the ones with errors.
export default function BrowserSessions(p: Props) {
  const { environment, window } = usePage<SharedProps>().props
  const a = environment!.application_id
  const e = environment!.id
  const href = (x: BrowserSession) =>
    R.applicationEnvironmentBrowserSessionPath(a, e, x.session_id)
  const visit = (next: { user?: string; errors?: boolean }) => {
    const user = next.user ?? p.user
    const errors = next.errors ?? p.errors
    router.visit(
      R.applicationEnvironmentBrowserSessionsPath(a, e, {
        window,
        ...(user ? { user } : {}),
        ...(errors ? { errors: 1 } : {}),
      }),
      { preserveState: true },
    )
  }
  return (
    <EnvLayout title="Sessions">
      <PageHeader
        title="Sessions"
        description="Browser sessions with the page visits and requests of every tab. One session is shared by a browser's tabs and ends after 30 minutes idle."
      />
      <div className="flex flex-wrap items-center gap-2">
        <FilterBar
          value={p.user ? `user:${p.user}` : ""}
          fields={[{ key: "user", label: "User" }]}
          placeholder="user:<ref>"
          onChange={(q) => {
            const m = /(?:^|\s)user:(\S+)/.exec(q)
            visit({ user: m ? m[1] : "" })
          }}
        />
        <Segmented>
          <SegmentedItem
            active={!p.errors}
            onClick={() => visit({ errors: false })}
          >
            All
          </SegmentedItem>
          <SegmentedItem
            active={p.errors}
            onClick={() => visit({ errors: true })}
          >
            With errors
          </SegmentedItem>
        </Segmented>
        {p.total > p.sessions.length && (
          <span className="text-muted-foreground text-xs">
            {count(p.sessions.length)} of {count(p.total)}
          </span>
        )}
      </div>
      <DataTable
        rows={p.sessions}
        rowKey={(x) => x.session_id}
        empty={
          <EmptyState
            icon={AppWindow}
            title="No browser sessions in this window"
            description="Sessions appear once the railwatch browser client tags page visits and requests with a session (railwatch gem 1.5 or later)."
          />
        }
        onRowClick={(x) => router.visit(href(x))}
        keyboardNav={{
          onOpen: (x, opts) =>
            opts?.newTab
              ? globalThis.open(href(x), "_blank")
              : router.visit(href(x)),
        }}
        columns={[
          {
            key: "s",
            header: "Session",
            cell: (x) => (
              <span className="font-mono text-xs">
                {x.session_id.slice(0, 12)}
              </span>
            ),
          },
          {
            key: "u",
            hideOnMobile: true,
            header: "User",
            cell: (x) => (
              <span className="font-mono text-xs">{x.users.join(", ")}</span>
            ),
          },
          {
            key: "t",
            header: "Tabs",
            align: "right",
            cell: (x) => count(x.tabs),
          },
          {
            key: "v",
            hideOnMobile: true,
            header: "Visits",
            align: "right",
            cell: (x) => count(x.visits),
          },
          {
            key: "r",
            header: "Requests",
            align: "right",
            cell: (x) => count(x.requests),
          },
          {
            key: "e",
            header: "Errors",
            align: "right",
            cell: (x) => (
              <span className={x.errors ? "text-destructive" : ""}>
                {x.errors}
              </span>
            ),
          },
          {
            key: "d",
            hideOnMobile: true,
            header: "Length",
            align: "right",
            cell: (x) => duration(x.first, x.last),
          },
          {
            key: "l",
            header: "Last active",
            align: "right",
            cell: (x) => ago(x.last),
          },
        ]}
      />
    </EnvLayout>
  )
}
