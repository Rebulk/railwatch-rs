import AppWordmark from "@/components/app-wordmark"
import { DOCS_URL, PRIVACY_URL, TERMS_URL } from "@/lib/site"

import { RebulkWordmark } from "./rebulk-wordmark"

export function MarketingFooter() {
  return (
    <footer className="mx-auto flex w-full max-w-5xl flex-col gap-3 border-t px-6 py-8 text-sm sm:flex-row sm:items-center sm:justify-between">
      <div className="flex items-center text-white">
        <AppWordmark className="h-4 w-auto" />
      </div>
      <p className="text-muted-foreground flex flex-wrap items-center gap-x-2 gap-y-1">
        <a href={DOCS_URL} className="hover:text-foreground">
          Docs
        </a>
        <span aria-hidden>·</span>
        <a href={TERMS_URL} className="hover:text-foreground">
          Terms
        </a>
        <span aria-hidden>·</span>
        <a href={PRIVACY_URL} className="hover:text-foreground">
          Privacy
        </a>
        <span aria-hidden>·</span>
        <span className="flex items-baseline gap-1.5">
          Built by
          <a
            href="https://rebulk.com"
            className="text-foreground/80 hover:text-foreground transition-colors"
          >
            <RebulkWordmark className="h-3 w-auto" />
          </a>
        </span>
        <span aria-hidden>·</span>© {new Date().getFullYear()}
      </p>
    </footer>
  )
}
