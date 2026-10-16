import { Card, CardContent, CardHeader } from "@/components/ui/card"

// What each half is is docs/embedded.md's "Three ways to run it" table, and
// what only the cloud has is its "Not in embedded mode" list. Keep all
// three in step.
const IN_APP = [
  "Every page: requests, jobs, queries, issues, logs, LLM calls, deploys",
  "Telemetry in two SQLite files next to your own databases",
  "Nothing leaves the machine; nothing touches your primary database",
  "Behind your own admin auth, or HTTP Basic out of the box",
]

const CLOUD = [
  "Alerts to Slack, email, webhooks, and Linear that still arrive when your app is down",
  "An MCP server, so your AI assistant can read production",
  "Every app, environment, and server in one place",
  "Accounts and team members, not one shared password",
]

const EXPORT_LINE = "c.export_enabled = true  # plus RAILWATCH_TOKEN"

export function DeploymentModes() {
  return (
    <section className="mx-auto max-w-4xl px-6 pb-16">
      <div className="text-center">
        <p className="text-primary mb-2 font-mono text-[11px] tracking-[0.2em] uppercase">
          Where your data lives
        </p>
        <h2 className="text-2xl font-bold tracking-tight">
          Local by default. Cloud when you want it.
        </h2>
        <p className="text-muted-foreground mx-auto mt-2 max-w-2xl text-balance">
          Railwatch runs inside your app and needs nothing else. Add Railwatch
          Cloud with one line and your app mirrors every record to it, while the
          dashboard at /railwatch keeps working.
        </p>
      </div>
      <div className="mt-8 grid gap-3 md:grid-cols-2">
        <ModeCard
          label="In your app"
          title="The whole dashboard at /railwatch"
          items={IN_APP}
        />
        <ModeCard
          label="Plus Railwatch Cloud"
          title="Uptime, alerts, and a team"
          items={CLOUD}
          footer={
            <p className="bg-muted mt-4 overflow-x-auto rounded p-3 font-mono text-xs">
              {EXPORT_LINE}
            </p>
          }
        />
      </div>
    </section>
  )
}

function ModeCard({
  label,
  title,
  items,
  footer,
}: {
  label: string
  title: string
  items: string[]
  footer?: React.ReactNode
}) {
  return (
    <Card className="gap-3">
      <CardHeader>
        <p className="text-primary font-mono text-[11px] tracking-[0.2em] uppercase">
          {label}
        </p>
        <h3 className="text-lg font-semibold">{title}</h3>
      </CardHeader>
      <CardContent>
        <ul className="text-muted-foreground space-y-2 text-sm">
          {items.map((item) => (
            <li key={item} className="flex gap-2">
              <span aria-hidden className="text-primary">
                ·
              </span>
              {item}
            </li>
          ))}
        </ul>
        {footer}
      </CardContent>
    </Card>
  )
}
