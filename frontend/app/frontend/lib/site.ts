// Where the product site's links go. The manual is the gem's docs on
// GitHub; the landing page and the hosted service's terms are the static
// site (frontend/site, deployed to railwatch.dev).
export const DOCS_URL = "https://github.com/Rebulk/railwatch/tree/main/docs#readme"
export const SELF_HOST_URL =
  "https://github.com/Rebulk/railwatch-rs/blob/main/docs/SELF-HOST.md"
export const GETTING_STARTED_URL =
  "https://github.com/Rebulk/railwatch/blob/main/docs/getting-started.md"
// Where the hosted app lives, for the static site's links into it
// (VITE_APP_URL, set by site/build.sh). "" inside the app: the same origin.
export const APP_URL = import.meta.env.VITE_APP_URL ?? ""
export const SITE_URL = "https://railwatch.dev"
export const TERMS_URL = `${SITE_URL}/legal/terms`
export const PRIVACY_URL = `${SITE_URL}/legal/privacy`
// Railwatch Cloud is invite-only for now, so "get started" asks for an
// invitation by email rather than opening a sign-up form that only
// redirects to sign-in. The address the terms and privacy policy name.
export const REQUEST_ACCESS_URL =
  "mailto:cole@rebulk.com?subject=Railwatch%20Cloud%20access"
