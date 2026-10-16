# Runs inside rw-baseline (bin/rails runner /tmp/mcp_capture.rb). POSTs each
# case in /tmp/mcp-cases.json to /mcp through ActionDispatch::Integration::
# Session (the real McpController, middleware and all) with the clock frozen
# at NOW, and writes {name => {status, body}} to /tmp/mcp-rails.json plus
# per-case sorted timings (ms) to /tmp/mcp-rails-timings.json.
#
# Tokens come from /tmp/mcp-tokens.json ({name => raw}). Write cases run
# once; read cases WARM+1 times (first dropped, Rails.cache cleared before
# each, as the props timings do). The preauth rate limit (300/min/IP) would
# trip on a long run, so every request comes from its own 10.x address.
#
# ENV: NOW (epoch float), WARM.
require "active_support/testing/time_helpers"
include ActiveSupport::Testing::TimeHelpers

cases = JSON.parse(File.read("/tmp/mcp-cases.json"))
tokens = JSON.parse(File.read("/tmp/mcp-tokens.json"))
now = Time.at(ENV.fetch("NOW").to_f).utc
warm = ENV.fetch("WARM", "0").to_i
out = {}
timings = {}
n = 0
travel_to(now) do
  s = ActionDispatch::Integration::Session.new(Rails.application)
  s.host! "localhost"
  cases.each do |c|
    headers = {"Content-Type" => c["content_type"] || "application/json"}
    raw = c["token"] && tokens[c["token"]]
    headers["Authorization"] = "Bearer #{raw}" if raw
    headers["Authorization"] = c["authorization"] if c["authorization"]
    body = c["body"].is_a?(String) ? c["body"] : JSON.generate(c["body"])
    runs = c["write"] ? 1 : warm + 1
    samples = []
    runs.times do
      Rails.cache.clear
      n += 1
      env = {"REMOTE_ADDR" => "10.#{(n >> 16) & 255}.#{(n >> 8) & 255}.#{n & 255}"}
      t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      s.post "/mcp", params: body, headers: headers, env: env
      samples << (Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0) * 1000.0
    end
    samples.shift if samples.size > 1
    timings[c["name"]] = samples.sort
    ctype = s.response.media_type
    out[c["name"]] = {"status" => s.response.status,
                      "body" => ctype == "application/json" ? JSON.parse(s.response.body) : nil}
  end
end
File.write("/tmp/mcp-rails.json", JSON.generate(out))
File.write("/tmp/mcp-rails-timings.json", JSON.generate(timings))
puts "CAPTURED #{out.size} cases"
