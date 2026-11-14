
import { CopyBlock } from "@/components/railwatch/copy-block"
import { GETTING_STARTED_URL } from "@/lib/site"

// Mirrors the gem's README and docs/getting-started.md. If the install
// there changes, change it here — this is the first thing anyone evaluating
// Railwatch reads.
const INSTALL = `bundle add railwatch
bin/rails generate railwatch:install --prompt-token`

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
        Paste the token from your environment's page and the generator wires
        up the rest. No Node, no Redis, no job worker, and nothing written to
        your database. For your own server, add{" "}
        <span className="text-foreground font-mono">--url</span>.
      </p>
      <CopyBlock code={INSTALL} className="mt-6 text-left [&_pre]:text-sm" />
      {/* Deploying with Kamal, Docker, Heroku or Render is covered in
          docs/getting-started.md rather than spelled out beside the two
          commands. */}
      <p className="mt-3 text-sm">
        <a
          href={GETTING_STARTED_URL}
          className="text-muted-foreground hover:text-foreground underline-offset-4 hover:underline"
        >
          Deploying with Kamal, Docker, Heroku, or Render? Read Getting started →
        </a>
      </p>
    </section>
  )
}
