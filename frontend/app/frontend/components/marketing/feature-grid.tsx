import { ChevronLeft, ChevronRight } from "lucide-react"
import { useRef, useState } from "react"

import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "@/components/ui/dialog"
import { ScrollArea, ScrollBar } from "@/components/ui/scroll-area"

// Each card shows a crop of the real page behind the feature, taken from a
// week of a demo storefront's telemetry; clicking it opens a tour of the
// features. Wide screens get the full page; phones get the crop at a readable
// size, panned sideways, because the full page shrunk to 390px is a smear.
// The images live in public/marketing so their URLs stay stable across
// asset digests, like the social preview image.
interface Feature {
  title: string
  description: string
  image: string
  alt: string
}

const FEATURES: Feature[] = [
  {
    title: "Requests",
    description:
      "Every route's throughput, latency percentiles, and status codes, down to a single request's waterfall.",
    image: "requests",
    alt: "The Requests page: throughput and latency charts over a table of every route with its 2xx, 4xx, 5xx counts, error rate, and p95",
  },
  {
    title: "Jobs & scheduled tasks",
    description:
      "Queue depth, retries, and run history for Active Job and your recurring tasks.",
    image: "jobs",
    alt: "The Jobs page: attempts per hour split into processed and failed, a queue table with average wait, and the most recent runs",
  },
  {
    title: "Queries & N+1s",
    description:
      "Slow queries and N+1 patterns grouped by source location, not buried in a log line.",
    image: "queries",
    alt: "The Queries page: two N+1 patterns with a suggested fix and the source line, above every SQL shape ranked by count and p95",
  },
  {
    title: "LLM calls",
    description:
      "RubyLLM calls with their tokens, cost, cut-offs, and tool calls, tied to the request or job that made them.",
    image: "llm",
    alt: "The LLM page: model and tool calls over the week, spend and tokens by model, and the request or job that made each call",
  },
  {
    title: "Exceptions → Issues",
    description:
      "Exceptions are grouped into Issues that track occurrences, assignees, and status over time.",
    image: "issues",
    alt: "An issue page: a resolved NoMethodError with the raising line of source highlighted, its breadcrumbs, and the alerts it sent",
  },
  {
    title: "Inertia visits",
    description:
      "Core Web Vitals and load time for every Inertia page, so a slow visit is obvious before support tickets show up.",
    image: "visits",
    alt: "The Inertia visits page: p75 web vitals, visits per hour, and a table of components with visit count, LCP, INP, and p95",
  },
  {
    title: "Deploy comparison",
    description:
      "Every chart is annotated with deploys, so a regression points straight at the change that caused it.",
    image: "deploys",
    alt: "A deploy page: the commits it shipped, its crash-free rate, and request p95, error rate, and job failures for the hour before and after",
  },
  {
    title: "MCP / API",
    description:
      "Your AI assistant reads the same timeline you do: stacks, breadcrumbs, query plans, and the deploy that caused it.",
    image: "mcp",
    alt: "A request page with its Copy as Markdown button: the same timeline an assistant reads over MCP",
  },
]

// The dashboard's own page colour, so the inset around each screenshot reads
// as more of the same page rather than a mat.
const DASHBOARD_BG = "bg-[#020618]"

export function FeatureGrid() {
  // The index outlives the dialog so its content stays put while it fades
  // out, and so focus can go back to the card of the last feature shown
  // (the cards are plain buttons, not DialogTriggers, so Radix can't).
  const [index, setIndex] = useState(0)
  const [isOpen, setIsOpen] = useState(false)
  const cards = useRef<(HTMLButtonElement | null)[]>([])
  const feature = FEATURES[index]

  function step(by: number) {
    setIndex((i) => (i + by + FEATURES.length) % FEATURES.length)
  }

  return (
    <section className="mx-auto max-w-5xl px-6 pb-16">
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
        {FEATURES.map(({ title, description, image, alt }, i) => (
          <div
            key={title}
            className="bg-card flex flex-col overflow-hidden rounded-lg border"
          >
            <button
              ref={(el) => {
                cards.current[i] = el
              }}
              type="button"
              onClick={() => {
                setIndex(i)
                setIsOpen(true)
              }}
              className={`group relative aspect-[4/3] w-full cursor-zoom-in overflow-hidden border-b px-4 pt-4 text-left ${DASHBOARD_BG}`}
              aria-label={`Open a screenshot of ${title}`}
            >
              <div
                className={`h-full overflow-hidden rounded-t-md pt-2.5 pl-2.5 ring-1 ring-white/10 transition-transform duration-500 ease-out group-hover:-translate-y-1 motion-reduce:transition-none ${DASHBOARD_BG}`}
              >
                <img
                  src={`/marketing/${image}.webp`}
                  alt={alt}
                  loading="lazy"
                  width={1200}
                  height={900}
                  className="h-full w-full object-cover object-left-top"
                />
              </div>
              {/* The crop is cut mid-table; fade it out instead of ending on
                  a half row. */}
              <div className="pointer-events-none absolute inset-x-0 bottom-0 h-1/4 bg-gradient-to-t from-[#020618] to-transparent" />
            </button>
            <div className="space-y-1 px-4 py-3">
              <h3 className="text-sm font-semibold">{title}</h3>
              <p className="text-muted-foreground text-[13px] leading-snug">
                {description}
              </p>
            </div>
          </div>
        ))}
      </div>

      <Dialog open={isOpen} onOpenChange={setIsOpen}>
        {/* The dialog portals out of the page's .dark wrapper, so it opts back
            in. Phones get the whole screen; wider screens get a box as wide as
            the image can be while the header still fits, so there are no
            letterbox gutters. */}
        <DialogContent
          className="dark bg-background text-foreground flex h-dvh max-h-none w-screen max-w-none flex-col gap-0 overflow-hidden rounded-none border-0 p-0 motion-reduce:animate-none sm:h-auto sm:max-h-[94vh] sm:w-[min(94vw,1440px,calc((94vh-4.75rem)*1.6))] sm:max-w-none sm:rounded-lg sm:border"
          onCloseAutoFocus={(e) => {
            e.preventDefault()
            cards.current[index]?.focus()
          }}
          onKeyDown={(e) => {
            // Arrows inside the phone crop pan it; only outside it do they
            // step to the next feature.
            if (
              e.target instanceof Element &&
              e.target.closest("[data-radix-scroll-area-viewport]")
            )
              return
            if (e.key === "ArrowRight") step(1)
            else if (e.key === "ArrowLeft") step(-1)
          }}
        >
          <div className="flex items-start gap-3 border-b py-3 pr-12 pl-5">
            <div className="min-w-0 flex-1 space-y-1">
              <DialogTitle className="text-base">{feature.title}</DialogTitle>
              <DialogDescription className="leading-snug">
                {feature.description}
              </DialogDescription>
            </div>
            <Pager className="hidden sm:flex" index={index} onStep={step} />
          </div>

          <ScrollArea
            key={feature.image}
            className={`[container-type:size] min-h-0 flex-1 sm:[container-type:normal] ${DASHBOARD_BG}`}
          >
            <picture>
              <source
                media="(min-width: 640px)"
                srcSet={`/marketing/${feature.image}-full.webp`}
                width={1920}
                height={1200}
              />
              {/* On a phone the crop fills the height between header and
                  pager (about two thirds of its pixel size, so the
                  dashboard text stays legible); the rest is a swipe
                  away. */}
              <img
                src={`/marketing/${feature.image}.webp`}
                alt={feature.alt}
                width={1200}
                height={900}
                className="block h-[100cqh] w-auto max-w-none p-3 sm:h-auto sm:w-full sm:p-0"
              />
            </picture>
            <ScrollBar orientation="horizontal" />
          </ScrollArea>

          <div className="flex items-center justify-between gap-3 border-t px-5 py-3 sm:hidden">
            <p className="text-muted-foreground text-xs">
              Swipe sideways for the rest of the page.
            </p>
            <Pager index={index} onStep={step} />
          </div>
        </DialogContent>
      </Dialog>
    </section>
  )
}

function Pager({
  index,
  onStep,
  className = "flex",
}: {
  index: number
  onStep: (by: number) => void
  className?: string
}) {
  return (
    <div className={`shrink-0 items-center gap-1 ${className}`}>
      <Button
        variant="outline"
        size="icon-sm"
        onClick={() => onStep(-1)}
        aria-label={`Previous: ${FEATURES[(index - 1 + FEATURES.length) % FEATURES.length].title}`}
      >
        <ChevronLeft />
      </Button>
      <span className="text-muted-foreground w-10 text-center text-xs tabular-nums">
        {index + 1} / {FEATURES.length}
      </span>
      <Button
        variant="outline"
        size="icon-sm"
        onClick={() => onStep(1)}
        aria-label={`Next: ${FEATURES[(index + 1) % FEATURES.length].title}`}
      >
        <ChevronRight />
      </Button>
    </div>
  )
}
