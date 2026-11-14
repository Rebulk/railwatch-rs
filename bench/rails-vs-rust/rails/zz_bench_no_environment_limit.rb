# frozen_string_literal: true

# Benchmark-only initializer, mounted read-only into the railwatch-cloud
# production image by bench/rails-vs-rust/lib.sh. It removes ONE thing: the
# per-environment ingest limit (600 requests/min, Ingest::BaseController),
# so a single app can be pushed past 10 batches/s, as the Rust side is
# (RAILWATCH_RATE_LIMIT_ENV=0). The IP backstop (IngestRequestRateLimit,
# 2,400/min per IP) stays on, on both sides; the load generator rotates
# X-Forwarded-For so it runs on every request but never trips.
#
# Removing it also removes one Solid Cache increment per request from Rails'
# measured path: this flatters Rails slightly.
Rails.application.config.to_prepare do
  Ingest::BaseController.class_eval do
    private def rate_limiting(**) = nil
  end
end
