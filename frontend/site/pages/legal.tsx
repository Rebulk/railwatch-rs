import { Head } from "@inertiajs/react"

import { MarketingFooter } from "@/components/marketing/footer"
import { MarketingNav } from "@/components/marketing/nav"

interface Props {
  title: string
  html: string
}

// The hosted service's terms and privacy policy on railwatch.dev, inside
// the landing page's nav and footer.
export default function LegalShow({ title, html }: Props) {
  return (
    <>
      <Head title={title} />
      <div className="dark bg-background text-foreground min-h-screen">
        <MarketingNav signedIn={false} />
        <article
          className="prose prose-neutral dark:prose-invert prose-headings:font-semibold prose-headings:tracking-tight prose-h1:text-3xl prose-a:text-primary mx-auto max-w-3xl px-6 pb-16"
          dangerouslySetInnerHTML={{ __html: html }}
        />
        <MarketingFooter />
      </div>
    </>
  )
}
