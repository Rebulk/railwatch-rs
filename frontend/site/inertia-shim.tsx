// What the landing page's components use from @inertiajs/react, for the
// static railwatch.dev build (vite.site.config.ts aliases the package here).
// There is no server and no session on the static site: nobody is signed
// in, links are plain anchors, and the live-update hooks have nothing to do.
import { type AnchorHTMLAttributes, type ReactNode, useEffect } from "react"

export function Head({ title }: { title?: string; children?: ReactNode }) {
  useEffect(() => {
    if (title) document.title = `${title} - Railwatch`
  }, [title])
  return null
}

export function Link(props: AnchorHTMLAttributes<HTMLAnchorElement>) {
  return <a {...props} />
}

export function usePage() {
  return { url: window.location.pathname, props: { auth: { user: null } } }
}

export const router = {
  on: () => () => {},
  reload: () => {},
}
