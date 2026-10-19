import { createInertiaApp } from "@inertiajs/react"
import { StrictMode } from "react"
import { createRoot } from "react-dom/client"

import { initializeTheme } from "@/hooks/use-appearance"
import PersistentLayout from "@/layouts/persistent-layout"
import { applyMount } from "@/lib/mount"
import { loadPage } from "@/lib/pages"
import { railwatchRootOptions, startRailwatch } from "@/lib/railwatch"

// Served under a base path (RAILWATCH_BASE_PATH), every route helper must
// carry it; the server emits it the way the gem's embedded engine does.
applyMount()

const appName = import.meta.env.VITE_APP_NAME ?? "Railwatch"

void createInertiaApp({
  // Set default page title
  // see https://inertia-rails.dev/guide/title-and-meta
  //
  title: (title) => (title ? `${title} - ${appName}` : appName),

  resolve: async (name) => {
    const page = await loadPage(name)

    // To use a default layout, import the Layout component
    // and use the following line.
    // see https://inertia-rails.dev/guide/pages#default-layouts
    //
    page.default.layout ??= [PersistentLayout]

    return page
  },

  setup({ el, App, props }) {
    // Uncomment the following to enable SSR hydration:
    // if (el.hasChildNodes()) {
    //   hydrateRoot(el, <App {...props} />)
    //   return
    // }
    createRoot(el, railwatchRootOptions()).render(
      <StrictMode>
        <App {...props} />
      </StrictMode>,
    )
  },

  defaults: {
    form: {
      forceIndicesArrayFormatInFormData: false,
      withAllErrors: true,
    },
    future: {
      useScriptElementForInitialPage: true,
      useDataInertiaHeadAttribute: true,
      useDialogForErrorModal: true,
      preserveEqualProps: true,
    },
  },

  progress: {
    color: "#4B5563",
  },
}).catch((error) => {
  // This ensures this entrypoint is only loaded on Inertia pages
  // by checking for the presence of the root element (#app by default).
  // Feel free to remove this `catch` if you don't need it.
  if (document.getElementById("app")) {
    throw error
  } else {
    console.error(
      "Missing root element.\n\n" +
        "If you see this error, it probably means you loaded Inertia.js on non-Inertia pages.\n" +
        'Consider moving <%= vite_typescript_tag "inertia" %> to the Inertia-specific layout instead.',
    )
  }
})

// This will set light / dark mode on load...
initializeTheme()

// Report Inertia visit timings to Railwatch (the platform monitors itself).
startRailwatch()
