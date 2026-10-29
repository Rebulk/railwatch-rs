import { Link, usePage } from "@inertiajs/react"
import { ChevronRight } from "lucide-react"
import { useState } from "react"

import { CopyId } from "@/components/railwatch/copy-id"
import { PageHeader } from "@/components/railwatch/page-header"
import { Stat, StatStrip } from "@/components/railwatch/stat"
import { KindBadge, StatusBadge } from "@/components/railwatch/status-badge"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import EnvLayout from "@/layouts/env-layout"
import { executionPath } from "@/lib/execution-path"
import { count, ms, when } from "@/lib/format"
import { foldRuns, type JourneyEvent, type Row } from "@/lib/journey"
import { cn } from "@/lib/utils"
import type { SharedProps } from "@/types"

interface Tab {
  id: string
  label: string
  first: string
  last: string
  requests: number
  visits: number
  errors: number
}
interface Props {
  session_id: string
  started: string
  last: string
  tabs: Tab[]
  events: JourneyEvent[]
  runs: number
  users: string[]
  ips: string[]
  user_agents: string[]
  errors: number
  capped: boolean
  cap: number
}

const typeLabel: Record<string, string> = {
  request: "REQ",
  visit: "PAGE",
  exception: "EXC",
  browser_error: "JS",
}

// Lane colours, cycled: each tab's marker and header chip. Not the status
// colours (amber, red): an error is the red ring on its marker.
const laneTones = ["bg-primary", "bg-live", "bg-chart-2", "bg-ok"]

function spanText(first: string, last: string): string {
  const s = Math.round((Date.parse(last) - Date.parse(first)) / 1000)
  if (s < 120) return `${s}s`
  if (s < 7200) return `${Math.floor(s / 60)}m ${s % 60}s`
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`
}

// A folded run's duration: the median, and the max when it differs.
// Events without one (exceptions, or a visit the client did not time) are
// left out; none at all shows nothing.
export function runDuration(events: JourneyEvent[]): string {
  const d = events
    .map((x) => x.duration ?? 0)
    .filter((x) => x > 0)
    .sort((a, b) => a - b)
  if (d.length === 0) return ""
  const p50 = d[Math.floor((d.length - 1) / 2)]
  const max = d[d.length - 1]
  return max === p50 ? ms(p50) : `${ms(p50)} · max ${ms(max)}`
}

function clock(iso: string): string {
  return new Date(iso).toISOString().slice(11, 19)
}

function label(e: JourneyEvent): string {
  switch (e.type) {
    case "request":
      return e.path ? `${e.name} ${e.path}` : (e.name ?? "")
    case "visit":
      return `${e.component ?? ""} ${e.url ?? ""}`.trim()
    default:
      return `${e.class_name}: ${e.message}`
  }
}

// One timeline row: the time, a cell per tab (the event's own lane marked),
// what happened, and its duration. A folded run says how many and over what.
function JourneyRow({
  row,
  lanes,
  open,
  onToggle,
  href,
}: {
  row: Row
  lanes: number
  open: boolean
  onToggle: () => void
  href: string | null
}) {
  const first = row.events[0]
  const folded = row.events.length > 1 && !open
  // A folded run of page visits each with its request (a page reloading on
  // a timer) leads with the request: "GET /three ×15", the visits counted
  // beside it. Otherwise the run's own events.
  const requests = row.events.filter((x) => x.type === "request")
  const paired =
    folded && requests.length > 0 && requests.length < row.events.length
  const e = paired ? requests[0] : first
  const shown = paired ? requests : row.events
  const n = shown.length
  const body = (
    <>
      <span className="text-muted-foreground font-mono tabular-nums">
        {clock(first.at)}
      </span>
      <span
        className="flex items-center gap-1"
        style={{ width: `${lanes + 3}rem` }}
        aria-hidden
      >
        {Array.from({ length: lanes }, (_, l) => (
          <span
            key={l}
            className={cn(
              "h-4 w-3 rounded-sm",
              l === e.lane ? laneTones[l % laneTones.length] : "bg-muted/40",
              l === e.lane &&
                row.events.some((x) => x.error) &&
                "ring-danger ring-2",
            )}
          />
        ))}
        {lanes > 0 && e.lane === null && (
          <span className="text-muted-foreground font-mono text-[10px]">
            no tab
          </span>
        )}
      </span>
      <span className="flex min-w-0 items-center gap-1.5">
        <KindBadge
          tone={
            e.error ? "destructive" : e.type === "visit" ? "success" : "muted"
          }
        >
          {typeLabel[e.type] ?? e.type}
        </KindBadge>
        {e.type === "request" && (
          <StatusBadge status={e.status} outcome={null} />
        )}
        <span className="truncate font-mono">{label(e)}</span>
        {/* On the folded row, and on an opened run's first row to fold it again. */}
        {row.size > 1 && (folded || (open && row.index === 0)) && (
          <button
            type="button"
            onClick={(ev) => {
              ev.preventDefault()
              ev.stopPropagation()
              onToggle()
            }}
            className="text-primary inline-flex shrink-0 items-center gap-0.5 font-mono text-[10px] hover:underline"
            aria-expanded={open}
            aria-label={`${open ? "Collapse" : "Expand"} ${label(e)} (${row.size} events)`}
          >
            <ChevronRight
              className={cn("size-3 transition-transform", open && "rotate-90")}
            />
            {open
              ? `×${row.size}`
              : n > 1
                ? `×${n} over ${spanText(shown[0].at, shown[n - 1].at)}`
                : ""}
          </button>
        )}
        {paired && (
          <span className="text-muted-foreground shrink-0 font-mono text-[10px]">
            + {row.events.length - n} page{" "}
            {row.events.length - n === 1 ? "visit" : "visits"}
          </span>
        )}
      </span>
      <span className="text-right font-mono whitespace-nowrap tabular-nums">
        {folded ? runDuration(shown) : e.duration ? ms(e.duration) : ""}
      </span>
    </>
  )
  const className =
    "grid grid-cols-[4.5rem_auto_1fr_auto] items-center gap-x-3 border-b px-2 py-1.5 text-xs last:border-b-0"
  return href && !folded ? (
    <Link href={href} className={cn(className, "hover:bg-muted/40")}>
      {body}
    </Link>
  ) : (
    <div className={className}>{body}</div>
  )
}

export default function BrowserSessionShow(p: Props) {
  const { environment } = usePage<SharedProps>().props
  const a = environment!.application_id
  const e = environment!.id
  const [openRuns, setOpenRuns] = useState<Record<number, boolean>>({})
  const rows = foldRuns(p.events, openRuns)
  const requests = p.events.filter((x) => x.type === "request").length
  const visits = p.events.filter((x) => x.type === "visit").length
  // The server's count (5xx, failed visits, browser errors): an exception
  // is drawn red on its request's row but is that request's error.
  const errors = p.errors
  const hrefOf = (x: JourneyEvent) =>
    x.execution_id
      ? executionPath({
          applicationId: a,
          environmentId: e,
          source: "request",
          executionId: x.execution_id,
        })
      : null

  return (
    <EnvLayout
      title="Session"
      crumbs={[
        { title: "Sessions", href: "#" },
        { title: "Session", href: "#" },
      ]}
    >
      <PageHeader
        withWindow={false}
        title={
          <span className="flex min-w-0 items-center gap-1">
            <span className="truncate font-mono text-base md:text-xl">
              {p.session_id}
            </span>
            <CopyId value={p.session_id} label="" />
          </span>
        }
        description={
          <span className="flex flex-wrap gap-x-3 text-xs">
            <span>{when(p.started)}</span>
            {p.users.length > 0 && <span>user {p.users.join(", ")}</span>}
            {p.ips.length > 0 && (
              <span>
                from {p.ips.slice(0, 3).join(", ")}
                {p.ips.length > 3 && ` and ${p.ips.length - 3} more`}
              </span>
            )}
          </span>
        }
      />
      <StatStrip>
        <Stat label="Length" value={spanText(p.started, p.last)} />
        <Stat
          label="Tabs"
          value={count(p.tabs.length)}
          hint={p.tabs.map((t) => t.label).join(" · ")}
        />
        <Stat label="Page visits" value={count(visits)} />
        <Stat label="Requests" value={count(requests)} />
        <Stat
          label="Errors"
          value={count(errors)}
          tone={errors ? "destructive" : undefined}
        />
      </StatStrip>
      {p.capped && (
        <p className="text-muted-foreground text-xs">
          This session has more than {count(p.cap)} requests or page visits: the
          newest are shown.
        </p>
      )}
      {p.tabs.length > 0 && (
        <div className="flex flex-wrap gap-2 text-xs">
          {p.tabs.map((t, l) => (
            <span
              key={t.id}
              className="bg-card inline-flex items-center gap-1.5 rounded-md border px-2 py-1"
              title={t.id}
            >
              <span
                className={cn(
                  "h-3 w-2 rounded-sm",
                  laneTones[l % laneTones.length],
                )}
              />
              <span className="font-mono font-medium">{t.label}</span>
              <span className="text-muted-foreground">
                {clock(t.first)}–{clock(t.last)} · {t.visits} pages ·{" "}
                {t.requests} requests
                {t.errors > 0 && (
                  <span className="text-destructive"> · {t.errors} errors</span>
                )}
              </span>
            </span>
          ))}
        </div>
      )}
      <Card>
        <CardHeader>
          <CardTitle>Journey</CardTitle>
        </CardHeader>
        <CardContent className="-mx-3 p-0 md:mx-0">
          {rows.map((row) => (
            <JourneyRow
              key={`${row.run}-${row.index}`}
              row={row}
              lanes={p.tabs.length}
              open={!!openRuns[row.run]}
              onToggle={() =>
                setOpenRuns((o) => ({ ...o, [row.run]: !o[row.run] }))
              }
              href={hrefOf(row.events[0])}
            />
          ))}
        </CardContent>
      </Card>
      {p.user_agents.length > 0 && (
        <p className="text-muted-foreground font-mono text-[11px] break-all">
          {p.user_agents.join(" · ")}
        </p>
      )}
    </EnvLayout>
  )
}
