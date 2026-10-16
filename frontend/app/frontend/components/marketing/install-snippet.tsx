import { Link } from "@inertiajs/react"

import { CopyBlock } from "@/components/railwatch/copy-block"
import { docPath } from "@/routes"

// Mirrors the gem's README and docs/embedded.md. If the install there
// changes, change it here — this is the first thing anyone evaluating
// Railwatch reads.
const INSTALL = `bundle add railwatch
bin/rails generate railwatch:install`

export function InstallSnippet() {
  return (
    <section
      id="install"
      className="mx-auto max-w-2xl scroll-mt-8 px-6 pb-16 text-center"
    >
      <p className="text-primary mb-2 font-mono text-[11px] tracking-[0.2em] uppercase">
        Install
      </p>
      <h2 className="text-2xl font-bold tracking-tight">Two commands</h2>
      <p className="text-muted-foreground mt-2 text-balance">
        No account, no token, no Node, no Redis, no job worker. The generator
        adds two SQLite databases and a writer process to Puma, and your app
        serves the whole dashboard at{" "}
        <span className="text-foreground font-mono">/railwatch</span>.
      </p>
      <CopyBlock code={INSTALL} className="mt-6 text-left [&_pre]:text-sm" />
      {/* Deploying, and apps on PostgreSQL or MySQL (one more bundle for
          the sqlite3 gem), are covered in docs/embedded.md rather than
          spelled out beside the two commands. */}
      <p className="mt-3 text-sm">
        <Link
          href={docPath("gem", "embedded")}
          className="text-muted-foreground hover:text-foreground underline-offset-4 hover:underline"
        >
          Deploying, or on PostgreSQL or MySQL? Read Embedded mode →
        </Link>
      </p>
    </section>
  )
}
