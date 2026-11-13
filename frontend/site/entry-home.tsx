import "./site.css"

import { StrictMode } from "react"
import { createRoot } from "react-dom/client"

import Home from "./pages/home"

createRoot(document.getElementById("app")!).render(
  <StrictMode>
    <Home />
  </StrictMode>,
)
