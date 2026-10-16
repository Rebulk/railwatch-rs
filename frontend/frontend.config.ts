// One dashboard frontend, two roots. The railwatch gem ships the dashboard's
// pages and components (app/frontend in the gem); this repository adds only
// what hosting needs: the marketing site, sign-in, accounts, settings, usage,
// and docs. `bin/rails railwatch:link_frontend` links tmp/railwatch-frontend
// at the bundled gem's copy, and `@/x` resolves to this app's app/frontend/x
// when it exists, otherwise the gem's. Vite and vitest share this file.
import { statSync } from "node:fs"
import { join } from "node:path"

import type { Plugin } from "vite"

const here = (path: string) => new URL(path, import.meta.url).pathname

export const cloudFrontend = here("./app/frontend")
export const gemFrontend = here("./tmp/railwatch-frontend")

const EXTENSIONS = ["", ".ts", ".tsx", ".js", ".css", "/index.ts", "/index.tsx"]

function locate(root: string, path: string) {
  return EXTENSIONS.map((ext) => join(root, path + ext)).find(
    (file) => statSync(file, { throwIfNoEntry: false })?.isFile() ?? false,
  )
}

// Where a bare import from a gem file is resolved from: a file of this
// app's own, so it finds this project's node_modules.
const localImporter = join(cloudFrontend, "entrypoints/inertia.tsx")

export function twoRoots(): Plugin {
  return {
    name: "railwatch-two-roots",
    enforce: "pre",
    async resolveId(source, importer, options) {
      // `@/` and `~/` are the only aliases the frontend uses. Vite's alias
      // plugin runs first and has usually rewritten `@/x` to this app's
      // app/frontend/x already, so both spellings land here.
      const alias =
        /^[@~]\/(.*)$/.exec(source) ??
        new RegExp(`^${cloudFrontend}/(.*)$`).exec(source)
      if (alias) {
        return (
          locate(cloudFrontend, alias[1]) ??
          locate(gemFrontend, alias[1]) ??
          null
        )
      }
      // A bare import (react, recharts) from a gem file resolves from this
      // project's node_modules. Vite follows the symlink, so the importer is
      // the gem's real path: outside this project when the gem is installed,
      // and under vendor/ when bundled from a path, where a checkout's own
      // node_modules would otherwise hand the page a second copy of React.
      //
      // railwatch-rs: only imports from the gem's files. railwatch-cloud's
      // copy also redirects bare imports made *inside node_modules* (e.g.
      // get-intrinsic -> es-errors); resolved from the entrypoint, a
      // transitive dependency is not in pnpm's top-level node_modules, so
      // Node's lookup walks up the directory tree and takes whatever copy
      // sits above the checkout (e.g. ~/src/node_modules): the build
      // then depended on where it ran. See docs/DIFFERENCES.md.
      const bare = /^[\w@]/.test(source)
      if (bare && importer && importer.startsWith(gemFrontend)) {
        return this.resolve(source, localImporter, {
          ...options,
          skipSelf: true,
        })
      }
      return null
    },
  }
}
