// Uptime page helpers (docs/uptime.md). The thresholds match the public
// status page (crates/server/src/web/status_page.rs) so a day reads the
// same colour in both.

export type DayTone = "none" | "up" | "partial" | "down"

export function dayTone(uptime: number | null): DayTone {
  if (uptime === null) return "none"
  if (uptime >= 100) return "up"
  if (uptime >= 95) return "partial"
  return "down"
}

// Never rounds up to 100%: 99.999 is "99.99%", not "100.00%".
export function uptimePct(value: number | null | undefined): string {
  if (value === null || value === undefined) return "–"
  if (value >= 100) return "100%"
  return `${(Math.floor(value * 100) / 100).toFixed(2)}%`
}

export function duration(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds))
  if (s < 60) return `${s} s`
  if (s < 3600) return `${Math.floor(s / 60)} min`
  if (s < 86400) {
    const h = Math.floor(s / 3600)
    const m = Math.floor((s % 3600) / 60)
    return m ? `${h} h ${m} min` : `${h} h`
  }
  const d = Math.floor(s / 86400)
  const h = Math.floor((s % 86400) / 3600)
  return h ? `${d} d ${h} h` : `${d} d`
}

export type Overall = "operational" | "degraded" | "down" | "unknown"

// Same rule as uptime::overall on the server.
export function overall(statuses: string[]): Overall {
  const checked = statuses.filter((s) => s === "up" || s === "down")
  const down = checked.filter((s) => s === "down").length
  if (checked.length === 0) return "unknown"
  if (down === 0) return "operational"
  return down === checked.length ? "down" : "degraded"
}

export const CAUSES: Record<string, string> = {
  dns: "DNS lookup failed",
  connect: "Connection failed",
  tls: "TLS error",
  timeout: "Timed out",
  status: "Unexpected status",
  keyword: "Keyword missing",
  blocked: "Address not allowed",
}
