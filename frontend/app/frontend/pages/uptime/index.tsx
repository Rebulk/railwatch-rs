import { Form, router, usePage } from "@inertiajs/react"
import { HeartPulse } from "lucide-react"
import { type ReactNode, useState } from "react"

import InputError from "@/components/input-error"
import { DataTable } from "@/components/railwatch/data-table"
import { EmptyState } from "@/components/railwatch/empty-state"
import { MetricChart } from "@/components/railwatch/metric-chart"
import { PageHeader } from "@/components/railwatch/page-header"
import { RelativeTime } from "@/components/railwatch/relative-time"
import { Segmented, SegmentedItem } from "@/components/railwatch/segmented"
import { Stat, StatStrip } from "@/components/railwatch/stat"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import EnvLayout from "@/layouts/env-layout"
import { ms } from "@/lib/format"
import {
  CAUSES,
  dayTone,
  duration,
  overall,
  uptimePct,
} from "@/lib/uptime"
import { cn } from "@/lib/utils"
import * as R from "@/routes"
import type { SharedProps } from "@/types"

export interface Day {
  date: string
  uptime: number | null
  checks: number
  failures: number
}

export interface Monitor {
  id: number
  name: string
  url: string
  method: string
  expected_status: number | null
  keyword: string | null
  timeout_seconds: number
  interval_seconds: number
  enabled: boolean
  public: boolean
  public_name: string | null
  public_url: boolean
  status: "pending" | "up" | "down" | "paused"
  status_since: string | null
  last_checked_at: string | null
  last_status_code: number | null
  last_latency_ms: number | null
  last_error: string | null
  uptime: Record<"24h" | "7d" | "30d" | "90d", number | null>
  days: Day[]
  latency: { t: string; value: number }[]
}

interface Incident {
  id: number
  monitor_id: number
  monitor: string
  started_at: string
  ended_at: string | null
  duration_seconds: number
  cause: string
  detail: string | null
}

interface Props {
  monitors: Monitor[]
  incidents: Incident[]
  status_page: {
    slug: string
    title: string
    enabled: boolean
    url: string
  } | null
  limits: {
    min_interval_seconds: number
    max_interval_seconds: number
    max_timeout_seconds: number
    max_monitors: number
    failures_to_down: number
  }
  can_manage: boolean
  window: string
}

const STATUS_LABEL: Record<Monitor["status"], string> = {
  up: "Up",
  down: "Down",
  pending: "Waiting for first check",
  paused: "Paused",
}

function StatusDot({ status }: { status: Monitor["status"] }) {
  return (
    <span
      aria-hidden
      className={cn(
        "inline-block size-2 shrink-0 rounded-full",
        status === "up" && "bg-live",
        status === "down" && "bg-danger",
        (status === "pending" || status === "paused") &&
          "bg-muted-foreground/40",
      )}
    />
  )
}

// 90 days, oldest left. Each bar's title says the day and its uptime.
export function DayBars({ days }: { days: Day[] }) {
  return (
    <div
      className="flex h-7 items-stretch gap-px"
      role="img"
      aria-label="Uptime per day, last 90 days"
    >
      {days.map((d, i) => {
        const tone = dayTone(d.uptime)
        return (
          <span
            key={d.date}
            title={`${d.date}: ${d.checks ? `${uptimePct(d.uptime)} (${d.failures} of ${d.checks} checks failed)` : "no checks"}`}
            data-tone={tone}
            className={cn(
              "min-w-0 flex-1 rounded-[1px]",
              tone === "none" && "bg-muted",
              tone === "up" && "bg-live",
              tone === "partial" && "bg-warning",
              tone === "down" && "bg-danger",
              // Phones show the last 30 days.
              i < days.length - 30 && "hidden md:block",
            )}
          />
        )
      })}
    </div>
  )
}

function MonitorCard({
  m,
  canManage,
  base,
}: {
  m: Monitor
  canManage: boolean
  base: { a: number; e: number }
}) {
  const [editing, setEditing] = useState(false)
  return (
    <Card className="min-w-0">
      <CardHeader className="gap-1">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <CardTitle className="flex min-w-0 items-center gap-2">
            <StatusDot status={m.status} />
            <span className="truncate">{m.name}</span>
            {m.public && <Badge variant="outline">public</Badge>}
          </CardTitle>
          <div className="text-muted-foreground flex items-center gap-2 text-xs">
            <span
              className={cn(
                m.status === "down" && "text-danger font-medium",
                m.status === "up" && "text-live",
              )}
            >
              {STATUS_LABEL[m.status]}
            </span>
            {m.status_since && (
              <span>
                since <RelativeTime iso={m.status_since} />
              </span>
            )}
          </div>
        </div>
        <div className="text-muted-foreground truncate font-mono text-xs">
          {m.method} {m.url} · every {m.interval_seconds} s
          {m.expected_status ? ` · expects ${m.expected_status}` : ""}
          {m.keyword ? ` · contains “${m.keyword}”` : ""}
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        <StatStrip>
          {(["24h", "7d", "30d", "90d"] as const).map((w) => (
            <Stat
              key={w}
              label={`Uptime ${w}`}
              value={uptimePct(m.uptime[w])}
              tone={
                m.uptime[w] === null
                  ? undefined
                  : m.uptime[w]! >= 99.9
                    ? "success"
                    : m.uptime[w]! >= 99
                      ? "warning"
                      : "destructive"
              }
            />
          ))}
          <Stat
            label="Last check"
            value={
              m.last_latency_ms !== null ? ms(m.last_latency_ms) : "–"
            }
            hint={
              m.last_checked_at ? (
                <>
                  {m.last_status_code ?? "no answer"} ·{" "}
                  <RelativeTime iso={m.last_checked_at} />
                </>
              ) : (
                "not checked yet"
              )
            }
          />
        </StatStrip>
        {m.last_error && m.status !== "up" && (
          <p className="text-danger font-mono text-xs">{m.last_error}</p>
        )}
        <DayBars days={m.days} />
        <div className="text-muted-foreground flex justify-between text-[11px]">
          <span className="hidden md:inline">90 days ago</span>
          <span className="md:hidden">30 days ago</span>
          <span>Today</span>
        </div>
        {m.latency.length > 0 ? (
          <MetricChart
            label="Response time"
            data={m.latency}
            format={(v) => ms(v)}
          />
        ) : (
          <p className="text-muted-foreground text-xs">
            No successful checks in this window yet.
          </p>
        )}
        {canManage && (
          <div className="flex flex-wrap gap-2">
            <Button
              size="sm"
              variant="outline"
              onClick={() => setEditing((v) => !v)}
            >
              {editing ? "Close" : "Edit"}
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={() =>
                router.patch(
                  R.applicationEnvironmentUptimeMonitorPath(
                    base.a,
                    base.e,
                    m.id,
                  ),
                  { enabled: !m.enabled },
                  { preserveScroll: true },
                )
              }
            >
              {m.enabled ? "Pause" : "Resume"}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="text-danger"
              onClick={() => {
                if (confirm(`Delete ${m.name} and its history?`))
                  router.delete(
                    R.applicationEnvironmentUptimeMonitorPath(
                      base.a,
                      base.e,
                      m.id,
                    ),
                    { preserveScroll: true },
                  )
              }}
            >
              Delete
            </Button>
          </div>
        )}
        {editing && (
          <MonitorForm
            monitor={m}
            action={R.applicationEnvironmentUptimeMonitorPath(
              base.a,
              base.e,
              m.id,
            )}
            method="patch"
            onDone={() => setEditing(false)}
          />
        )}
      </CardContent>
    </Card>
  )
}

function Field({
  id,
  label,
  error,
  className,
  children,
}: {
  id: string
  label: string
  error?: string[]
  className?: string
  children: ReactNode
}) {
  return (
    <div className={cn("grid gap-1", className)}>
      <Label htmlFor={id}>{label}</Label>
      {children}
      <InputError messages={error} />
    </div>
  )
}

function MonitorForm({
  monitor,
  action,
  method,
  onDone,
}: {
  monitor?: Monitor
  action: string
  method: "post" | "patch"
  onDone?: () => void
}) {
  const p = monitor
  const id = (k: string) => `${k}-${p?.id ?? "new"}`
  return (
    <Form
      method={method}
      action={action}
      options={{ preserveScroll: true }}
      onSuccess={onDone}
      resetOnSuccess={!p}
      className="grid gap-3 md:grid-cols-6"
    >
      {({ errors, processing }) => (
        <>
          <Field
            id={id("name")}
            label="Name"
            error={errors.name}
            className="md:col-span-2"
          >
            <Input
              id={id("name")}
              name="name"
              defaultValue={p?.name}
              placeholder="Storefront"
              required
            />
          </Field>
          <Field
            id={id("url")}
            label="URL"
            error={errors.url}
            className="md:col-span-4"
          >
            <Input
              id={id("url")}
              name="url"
              type="url"
              defaultValue={p?.url}
              placeholder="https://example.com/up"
              required
            />
          </Field>
          <Field id={id("method")} label="Method" error={errors.method}>
            <select
              id={id("method")}
              name="method"
              defaultValue={p?.method ?? "GET"}
              className="border-input h-9 rounded-md border bg-transparent px-2 text-sm"
            >
              <option value="GET">GET</option>
              <option value="HEAD">HEAD</option>
            </select>
          </Field>
          <Field
            id={id("expected_status")}
            label="Expected status"
            error={errors.expected_status}
          >
            <Input
              id={id("expected_status")}
              name="expected_status"
              inputMode="numeric"
              defaultValue={p?.expected_status ?? ""}
              placeholder="2xx/3xx"
            />
          </Field>
          <Field
            id={id("keyword")}
            label="Body contains"
            error={errors.keyword}
            className="md:col-span-2"
          >
            <Input
              id={id("keyword")}
              name="keyword"
              defaultValue={p?.keyword ?? ""}
              placeholder="optional"
            />
          </Field>
          <Field
            id={id("interval_seconds")}
            label="Every (s)"
            error={errors.interval_seconds}
          >
            <Input
              id={id("interval_seconds")}
              name="interval_seconds"
              type="number"
              min={30}
              max={3600}
              defaultValue={p?.interval_seconds ?? 60}
            />
          </Field>
          <Field
            id={id("timeout_seconds")}
            label="Timeout (s)"
            error={errors.timeout_seconds}
          >
            <Input
              id={id("timeout_seconds")}
              name="timeout_seconds"
              type="number"
              min={1}
              max={30}
              defaultValue={p?.timeout_seconds ?? 10}
            />
          </Field>
          <div className="flex flex-wrap items-center gap-4 md:col-span-6">
            {/* Unchecked checkboxes are not sent: these say "off" first,
                and a checked box after them wins. */}
            <input type="hidden" name="public" value="0" />
            <input type="hidden" name="public_url" value="0" />
            <label className="flex items-center gap-2 text-sm">
              <Checkbox name="public" value="1" defaultChecked={p?.public} />
              Show on the status page
            </label>
            <label className="flex items-center gap-2 text-sm">
              <Checkbox
                name="public_url"
                value="1"
                defaultChecked={p?.public_url}
              />
              Show its URL there
            </label>
            <Input
              name="public_name"
              defaultValue={p?.public_name ?? ""}
              placeholder="Public name (default: the name)"
              aria-label="Public name"
              className="max-w-xs"
            />
            <InputError messages={errors.public_name} />
          </div>
          <div className="md:col-span-6">
            <Button type="submit" size="sm" disabled={processing}>
              {p ? "Save monitor" : "Add monitor"}
            </Button>
          </div>
        </>
      )}
    </Form>
  )
}

function StatusPageCard({
  page,
  base,
  canManage,
}: {
  page: Props["status_page"]
  base: { a: number; e: number }
  canManage: boolean
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Public status page</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        <p className="text-muted-foreground">
          {page?.enabled ? (
            <>
              Live at{" "}
              <a
                href={page.url}
                target="_blank"
                rel="noreferrer"
                className="font-mono underline"
              >
                {page.url}
              </a>
              . It lists only monitors marked public, by their public name.
            </>
          ) : (
            "Off. When on, anyone with the link sees the monitors you mark public: their public name, status, 90 days of uptime and incident times. Never internal URLs or errors."
          )}
        </p>
        {canManage && (
          <Form
            method="patch"
            action={R.applicationEnvironmentUptimeStatusPagePath(
              base.a,
              base.e,
            )}
            options={{ preserveScroll: true }}
            className="grid gap-3 md:grid-cols-6"
          >
            {({ errors, processing }) => (
              <>
                <Field
                  id="sp-title"
                  label="Title"
                  error={errors.title}
                  className="md:col-span-3"
                >
                  <Input
                    id="sp-title"
                    name="title"
                    defaultValue={page?.title ?? ""}
                    placeholder="Acme status"
                  />
                </Field>
                <Field
                  id="sp-slug"
                  label="Address (/status/…)"
                  error={errors.slug}
                  className="md:col-span-3"
                >
                  <Input
                    id="sp-slug"
                    name="slug"
                    defaultValue={page?.slug ?? ""}
                    placeholder="blank: an unguessable one"
                  />
                </Field>
                <div className="flex items-center gap-4 md:col-span-6">
                  <input type="hidden" name="enabled" value="0" />
                  <label className="flex items-center gap-2">
                    <Checkbox
                      name="enabled"
                      value="1"
                      defaultChecked={page?.enabled}
                    />
                    Public
                  </label>
                  <Button type="submit" size="sm" disabled={processing}>
                    Save
                  </Button>
                </div>
              </>
            )}
          </Form>
        )}
      </CardContent>
    </Card>
  )
}

export default function Uptime(p: Props) {
  const { environment } = usePage<SharedProps>().props
  const a = environment!.application_id
  const e = environment!.id
  const base = { a, e }
  const statuses = p.monitors.filter((m) => m.enabled).map((m) => m.status)
  const state = overall(statuses)
  const down = statuses.filter((s) => s === "down").length
  const chartWindow = p.window === "7d" ? "7d" : "24h"
  return (
    <EnvLayout title="Uptime" crumbs={[{ title: "Uptime", href: "#" }]}>
      <PageHeader
        withWindow={false}
        title="Uptime"
        description={`This server requests each URL on its interval. A monitor is down after ${p.limits.failures_to_down} failed checks in a row and up after one success; both send monitor_down / monitor_recovered alerts.`}
        actions={
          <Segmented>
            {(["24h", "7d"] as const).map((w) => (
              <SegmentedItem
                key={w}
                active={chartWindow === w}
                onClick={() =>
                  router.get(
                    R.applicationEnvironmentUptimePath(a, e),
                    { window: w },
                    { preserveScroll: true, preserveState: true },
                  )
                }
              >
                {w}
              </SegmentedItem>
            ))}
          </Segmented>
        }
      />
      {p.monitors.length > 0 && (
        <StatStrip>
          <Stat
            label="Status"
            value={
              state === "operational"
                ? "All up"
                : state === "degraded"
                  ? `${down} down`
                  : state === "down"
                    ? "All down"
                    : "Waiting"
            }
            tone={
              state === "operational"
                ? "success"
                : state === "unknown"
                  ? undefined
                  : "destructive"
            }
          />
          <Stat label="Monitors" value={p.monitors.length} />
          <Stat
            label="Open incidents"
            value={p.incidents.filter((i) => !i.ended_at).length}
          />
        </StatStrip>
      )}
      {p.monitors.length === 0 ? (
        <Card>
          <EmptyState
            icon={HeartPulse}
            title="No monitors yet"
            description={
              p.can_manage
                ? "Add a URL below. Railwatch checks it from this server and alerts when it stops answering."
                : "An account admin can add monitors here."
            }
          />
        </Card>
      ) : (
        <div className="grid gap-4">
          {p.monitors.map((m) => (
            <MonitorCard
              key={m.id}
              m={m}
              canManage={p.can_manage}
              base={base}
            />
          ))}
        </div>
      )}
      <Card>
        <CardHeader>
          <CardTitle>Recent incidents</CardTitle>
        </CardHeader>
        <CardContent>
          <DataTable
            rows={p.incidents}
            rowKey={(i) => i.id}
            empty="No incidents in the last 90 days."
            columns={[
              {
                key: "monitor",
                header: "Monitor",
                grow: true,
                cell: (i) => <span className="font-medium">{i.monitor}</span>,
              },
              {
                key: "started",
                header: "Started",
                cell: (i) => <RelativeTime iso={i.started_at} />,
              },
              {
                key: "duration",
                header: "Duration",
                align: "right",
                cell: (i) =>
                  i.ended_at ? (
                    duration(i.duration_seconds)
                  ) : (
                    <span className="text-danger font-medium">
                      ongoing, {duration(i.duration_seconds)}
                    </span>
                  ),
              },
              {
                key: "cause",
                header: "Cause",
                hideOnMobile: true,
                cell: (i) => (
                  <span title={i.detail ?? undefined}>
                    {CAUSES[i.cause] ?? i.cause}
                  </span>
                ),
              },
            ]}
          />
        </CardContent>
      </Card>
      {p.can_manage && (
        <Card>
          <CardHeader>
            <CardTitle>Add a monitor</CardTitle>
          </CardHeader>
          <CardContent>
            {p.monitors.length >= p.limits.max_monitors ? (
              <p className="text-muted-foreground text-sm">
                This environment has the maximum of {p.limits.max_monitors}{" "}
                monitors.
              </p>
            ) : (
              <MonitorForm
                action={R.applicationEnvironmentUptimeMonitorsPath(a, e)}
                method="post"
              />
            )}
          </CardContent>
        </Card>
      )}
      <StatusPageCard
        page={p.status_page}
        base={base}
        canManage={p.can_manage}
      />
    </EnvLayout>
  )
}
