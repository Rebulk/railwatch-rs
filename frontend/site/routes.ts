// The route helpers the landing page's components call, for the static
// railwatch.dev build (vite.site.config.ts aliases @/routes here): the
// site's own root, and everything else on the hosted app.
import { APP_URL } from "@/lib/site"

export const rootPath = () => "/"
export const signInPath = () => `${APP_URL}/sign_in`
export const signUpPath = () => `${APP_URL}/sign_up`
export const dashboardPath = () => `${APP_URL}/dashboard`
