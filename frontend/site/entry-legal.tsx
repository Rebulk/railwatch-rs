import "./site.css"

import { StrictMode } from "react"
import { createRoot } from "react-dom/client"

import LegalShow from "./pages/legal"

// The hosted service's terms and privacy policy: railwatch-cloud's
// docs/legal Markdown rendered once to HTML, chosen by the page's data-legal.
import privacy from "./legal-html/privacy.html?raw"
import terms from "./legal-html/terms.html?raw"

const el = document.getElementById("app")!
const page =
  el.dataset.legal === "privacy"
    ? { title: "Privacy Policy", html: privacy }
    : { title: "Terms of Service", html: terms }

createRoot(el).render(
  <StrictMode>
    <LegalShow {...page} />
  </StrictMode>,
)
