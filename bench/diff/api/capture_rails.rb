# Runs inside rw-baseline (bin/rails runner /tmp/api_capture.rb). Sends
# each case in /tmp/api-cases.json through ActionDispatch::Integration::
# Session (routing, the whole middleware stack, the real Api::V1
# controllers) with the clock frozen at NOW, and writes {name => {status,
# headers, body}} to /tmp/api-rails.json plus per-case sorted timings (ms)
# to /tmp/api-rails-timings.json.
#
# A case: {name, method, path, token?, authorization?, headers?, body?,
# content_type?, write?, burst?, cursor_from?}. Tokens come from
# /tmp/api-tokens.json. `cursor_from: CASE` appends &cursor= with that
# earlier case's own next_cursor (each server signs with its own key).
# Rails.cache is cleared before each run (the rate-limit counters and the
# grouped/summary minute cache), so every run is computed, as in Rust;
# `burst: N` sends the request N times first without clearing (the rate
# limit case). Write cases run once; read cases WARM+1 times, first dropped.
#
# ENV: NOW (epoch), WARM.
require "active_support/testing/time_helpers"
include ActiveSupport::Testing::TimeHelpers

cases = JSON.parse(File.read("/tmp/api-cases.json"))
tokens = JSON.parse(File.read("/tmp/api-tokens.json"))
now = Time.at(ENV.fetch("NOW").to_f).utc
warm = ENV.fetch("WARM", "0").to_i
out = {}
timings = {}
travel_to(now) do
  s = ActionDispatch::Integration::Session.new(Rails.application)
  s.host! "localhost"
  cases.each do |c|
    headers = {}
    raw = c["token"] && tokens[c["token"]]
    headers["Authorization"] = "Bearer #{raw}" if raw
    headers["Authorization"] = c["authorization"] if c["authorization"]
    headers.merge!(c["headers"] || {})
    env = {}
    body = c["body"].nil? ? nil : (c["body"].is_a?(String) ? c["body"] : JSON.generate(c["body"]))
    if body
      env["CONTENT_TYPE"] = c["content_type"] || "application/json"
      env["CONTENT_LENGTH"] = body.bytesize.to_s
      env["rack.input"] = StringIO.new(body)
      env["RAW_POST_DATA"] = body
    end
    path = c["path"]
    if c["cursor_from"]
      cursor = out.dig(c["cursor_from"], "body", "results_meta", "next_cursor").to_s
      path = "#{path}&cursor=#{CGI.escape(cursor)}"
    end
    method = c["method"].downcase.to_sym
    send_one = lambda do
      env["rack.input"]&.rewind
      s.process(method, path, params: nil, headers: headers, env: env.dup)
    end
    runs = c["write"] ? 1 : warm + 1
    samples = []
    runs.times do
      Rails.cache.clear
      c.fetch("burst", 0).times { send_one.call }
      t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      send_one.call
      samples << (Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0) * 1000.0
    end
    samples.shift if samples.size > 1
    timings[c["name"]] = samples.sort
    text = s.response.body.to_s
    parsed = begin
      JSON.parse(text)
    rescue JSON::ParserError
      text
    end
    out[c["name"]] = {"status" => s.response.status, "headers" => s.response.headers.to_h.transform_keys(&:downcase), "body" => parsed}
  end
end
File.write("/tmp/api-rails.json", JSON.generate(out))
File.write("/tmp/api-rails-timings.json", JSON.generate(timings))
puts "CAPTURED #{out.size} cases"
