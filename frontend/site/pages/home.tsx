import { Head } from "@inertiajs/react"

import { AiAssistant } from "@/components/marketing/ai-assistant"
import { DeploymentModes } from "@/components/marketing/deployment-modes"
import { FeatureGrid } from "@/components/marketing/feature-grid"
import { MarketingFooter } from "@/components/marketing/footer"
import { MarketingHero } from "@/components/marketing/hero"
import { HeroStageProvider } from "@/components/marketing/hero-line"
import { InstallSnippet } from "@/components/marketing/install-snippet"
import { MarketingNav } from "@/components/marketing/nav"
import { ProductPreview } from "@/components/marketing/product-preview"
import { Reveal } from "@/components/marketing/reveal"

// railwatch.dev's landing page. A static page knows no session, so it is
// always the signed-out view; the app's own `/` sends a signed-in user to
// the dashboard.
export default function Welcome() {
  const signedIn = false

  return (
    <>
      <Head title="Monitoring Rails deserves" />
      {/* The marketing page is a brand page, so it always renders in the navy
          stack regardless of the signed-in appearance setting. */}
      <div className="dark bg-background text-foreground min-h-screen">
        <Reveal step={0}>
          <MarketingNav signedIn={signedIn} />
        </Reveal>
        <HeroStageProvider>
          <Reveal step={1}>
            <MarketingHero signedIn={signedIn} />
          </Reveal>
          <Reveal step={2}>
            <ProductPreview />
          </Reveal>
        </HeroStageProvider>
        <Reveal step={3}>
          <InstallSnippet />
        </Reveal>
        <Reveal step={4}>
          <FeatureGrid />
        </Reveal>
        <Reveal step={4}>
          <DeploymentModes />
        </Reveal>
        <Reveal step={4}>
          <AiAssistant />
        </Reveal>
        <Reveal step={5}>
          <MarketingFooter />
        </Reveal>
      </div>
    </>
  )
}
