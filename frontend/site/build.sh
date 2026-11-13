#!/usr/bin/env bash
# Build railwatch.dev and, with --deploy, upload it to Cloudflare Pages
# (project railwatch-site, custom domain railwatch.dev).
#
#   frontend/site/build.sh             build into frontend/dist-site
#   frontend/site/build.sh --deploy    build, then deploy to production
#
# Deploying needs CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID for the
# account that owns the project.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

export VITE_APP_URL="${VITE_APP_URL:-https://railwatch.rebulk.com}"
pnpm exec vite build --config vite.site.config.ts

# The app's icons (the screenshots and og.png come from site/public).
cp app/frontend/public/icon.png app/frontend/public/icon.svg dist-site/
# A 404.html turns off Pages' single-page fallback, so a missing asset is a
# 404 rather than index.html served (and cached for a year) as JavaScript.
cp site/404.html dist-site/
cat > dist-site/_headers <<'HEADERS'
/assets/*
  Cache-Control: public, max-age=31536000, immutable
/marketing/*
  Cache-Control: public, max-age=86400
HEADERS

if [ "${1:-}" = "--deploy" ]; then
  pnpm dlx wrangler@4 pages deploy dist-site --project-name railwatch-site --branch main --commit-dirty=true
fi
