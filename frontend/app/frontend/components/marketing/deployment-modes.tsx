import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader } from "@/components/ui/card"
import { REQUEST_ACCESS_URL, SELF_HOST_URL } from "@/lib/site"

// The three ways to run the one server, as the gem's README lists them:
// Railwatch Cloud, self-hosted railwatch-rs, and embedded railwatch-server.
// Keep these in step with that README and docs/self-hosting.md.
const CLOUD = [
  "Nothing to run: we keep the server up, backed up, and upgraded",
  "Alerts to Slack, email, and webhooks that still arrive when your app or its server is down",
  "Every app, environment, and server in one place, with a team",
  "Uptime monitors and a public status page",
]

const SELF_HOSTED = [
  "One static binary and SQLite on a small VPS, for all your apps",
  "HTTPS with Let's Encrypt built in, no proxy or database to install",
  "Upgrades that drop no data, and S3 backups",
  "Source-available (FSL), so you can read every line that touches your data",
]

const EMBEDDED = [
  "The same server, started by Puma next to one app",
  "The dashboard at /railwatch, behind your own admin login",
  "No token: it sets itself up on first boot",
  "Goes down with your app, so pair it with Cloud or self-hosted for alerts",
]

const SELF_HOST_LINE = `curl -fsSL https://github.com/Rebulk/railwatch-rs/releases/latest/download/install.sh | sudo sh -s -- --domain monitor.example.com

# later, to update:
sudo railwatch upgrade`

const EMBEDDED_LINE = `gem "railwatch-server"   # config/puma.rb: plugin :railwatch_server`

export function DeploymentModes() {
  return (
    <section className="mx-auto max-w-5xl px-6 pb-16">
      <div className="text-center">
        <p className="text-primary mb-2 font-mono text-[11px] tracking-[0.2em] uppercase">
          Where it runs
        </p>
        <h2 className="text-2xl font-bold tracking-tight">
          One server, three ways to run it.
        </h2>
        <p className="text-muted-foreground mx-auto mt-2 max-w-2xl text-balance">
          The gem in your app reports to a Railwatch server. Use ours, run the
          same server on your own box, or start it next to your app. Your
          monitoring should live somewhere other than the thing it watches, so
          when your app goes down, the alert still goes out.
        </p>
      </div>
      <div className="mt-8 grid gap-3 md:grid-cols-3">
        <ModeCard
          label="Railwatch Cloud"
          title="We run it"
          items={CLOUD}
          footer={
            <Button asChild size="sm" className="mt-4 w-full">
              <a href={REQUEST_ACCESS_URL}>Request access</a>
            </Button>
          }
        />
        <ModeCard
          label="Self-hosted"
          title="You run it, on your server"
          items={SELF_HOSTED}
          footer={
            <>
              <CodeLine code={SELF_HOST_LINE} />
              <a
                href={SELF_HOST_URL}
                className="text-muted-foreground hover:text-foreground mt-3 block text-sm underline-offset-4 hover:underline"
              >
                Self-hosting guide →
              </a>
            </>
          }
        />
        <ModeCard
          label="Embedded"
          title="Inside one app"
          items={EMBEDDED}
          footer={<CodeLine code={EMBEDDED_LINE} />}
        />
      </div>
    </section>
  )
}

function CodeLine({ code }: { code: string }) {
  return (
    <p className="bg-muted mt-4 rounded p-3 font-mono text-xs break-all whitespace-pre-wrap">
      {code}
    </p>
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
