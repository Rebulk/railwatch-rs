import { CopyBlock } from "@/components/railwatch/copy-block"
import { Card, CardContent } from "@/components/ui/card"

// The MCP one-liner is the same command bin/rails railwatch:mcp prints and the
// profile page hands you with a real token in it.
const CLAUDE_CODE_COMMAND =
  'claude mcp add railwatch --transport http https://railwatch.rebulk.com/mcp --header "Authorization: Bearer $RAILWATCH_MCP_TOKEN"'

// A sample of McpController::TOOLS and PROMPTS, by their real names.
const WHAT_IT_READS: { name: string; does: string }[] = [
  {
    name: "get_issue",
    does: "the full stack, cause, locals, and browser breadcrumbs of an exception",
  },
  {
    name: "get_execution",
    does: "one request or job with every query, log line, and call inside it",
  },
  {
    name: "get_route",
    does: "a route's percentiles, slowest queries, and N+1s with the fix",
  },
  {
    name: "explain_query",
    does: "the stored query plan for a slow SQL shape",
  },
  {
    name: "triage_issue",
    does: "a prompt: occurrence, timeline, and the deploy it started after",
  },
  {
    name: "update_issue",
    does: "resolve, assign, or comment, signed with your name in the activity feed",
  },
]

export function AiAssistant() {
  return (
    <section className="mx-auto max-w-3xl px-6 pb-16 text-center">
      <p className="text-primary mb-2 font-mono text-[11px] tracking-[0.2em] uppercase">
        MCP · Railwatch Cloud
      </p>
      <h2 className="text-2xl font-bold tracking-tight">
        Ask your assistant what broke
      </h2>
      <p className="text-muted-foreground mx-auto mt-2 max-w-2xl text-balance">
        Railwatch Cloud is an MCP server. Claude Code, Cursor, VS Code, and Zed
        read your production data directly: 17 tools and 3 prompts over issues,
        routes, query plans, stack profiles, logs, releases, and deploys.
      </p>
      <Card className="mt-6 text-left">
        <CardContent className="space-y-4">
          <dl className="grid gap-x-6 gap-y-2 text-sm sm:grid-cols-2">
            {WHAT_IT_READS.map(({ name, does }) => (
              <div key={name}>
                <dt className="text-primary font-mono text-xs">{name}</dt>
                <dd className="text-muted-foreground">{does}</dd>
              </div>
            ))}
          </dl>
          <div>
            <p className="text-muted-foreground font-mono text-xs">
              # one line, then ask it what broke today
            </p>
            <CopyBlock
              code={CLAUDE_CODE_COMMAND}
              className="mt-1.5 [&_pre]:break-all [&_pre]:whitespace-pre-wrap"
            />
          </div>
        </CardContent>
      </Card>
    </section>
  )
}
