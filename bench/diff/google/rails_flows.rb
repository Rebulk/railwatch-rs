# frozen_string_literal: true

# The sign-in flows of bench/diff/google/cases.json through railwatch-cloud
# (rw-baseline), against mock_google.py. Run with GOOGLE_CLIENT_ID/SECRET set
# (so the OmniAuth middleware is registered) and DATABASE_URL pointing at a
# scratch primary DB; see run.sh. Prints one JSON line per case.
#
# The strategy is unchanged except where it talks to Google: authorize,
# token, userinfo and tokeninfo go to the mock.
require "action_dispatch/testing/integration"
require "net/http"

MOCK = ENV.fetch("MOCK_GOOGLE")
CASES = JSON.parse(File.read(ENV.fetch("CASES")))

OmniAuth::Strategies::GoogleOauth2.prepend(Module.new do
  def client
    ::OAuth2::Client.new(options.client_id, options.client_secret,
      site: MOCK, authorize_url: "#{MOCK}/authorize", token_url: "#{MOCK}/token")
  end

  # The gem's token_info with the tokeninfo URL swapped for the mock's.
  def token_info(access_token)
    return nil unless access_token

    @token_info ||= Hash.new do |h, k|
      h[k] = client.request(:post, "#{MOCK}/tokeninfo", body: {access_token: k}).parsed
    end
    @token_info[access_token]
  end
end)
OmniAuth::Strategies::GoogleOauth2.send(:remove_const, :USER_INFO_URL)
OmniAuth::Strategies::GoogleOauth2.const_set(:USER_INFO_URL, "#{MOCK}/userinfo")

def session
  s = ActionDispatch::Integration::Session.new(Rails.application)
  s.host = "localhost"
  s.https!
  s
end

def mock_next(nxt)
  claims = {"iss" => "https://accounts.google.com"}.merge(nxt["claims"] || {})
  Net::HTTP.post(URI("#{MOCK}/next"), JSON.generate(nxt.merge("claims" => claims)), "Content-Type" => "application/json")
end

def csrf(s)
  s.get "/sign_in"
  s.response.body[/name="csrf-token" content="([^"]+)"/, 1]
end

# The request phase; returns the provider's redirect back to us (a path).
def click_google(s, token: nil)
  token ||= csrf(s)
  s.post "/auth/google_oauth2", params: {authenticity_token: token}
  return [s.response.status, s.response.location] unless s.response.status == 302 && s.response.location.start_with?(MOCK)
  res = Net::HTTP.get_response(URI(s.response.location))
  [res.code.to_i, res["location"]]
end

def path_of(url)
  return nil unless url
  u = URI(url)
  u.query ? "#{u.path}?#{u.query}" : u.path
end

# GET a path, then follow our redirects to the page the user sees.
def land(s, path)
  s.get path
  first = s.response.location && path_of(s.response.location)
  first_flash = s.flash.to_h.slice("notice", "alert")
  hops = 0
  while s.response.redirect? && hops < 5
    s.follow_redirect!
    hops += 1
  end
  page = s.response.body[%r{<script data-page="app" type="application/json"[^>]*>(.*?)</script>}m, 1]
  props = page ? JSON.parse(page)["props"] : {}
  {first: first, first_flash: first_flash, final: s.request.path, flash: (props["flash"] || {}).compact.reject { |_, v| v.nil? }}
end

def users
  User.order(:id).map { |u| [u.email, u.provider, u.uid, u.verified] }
end

def setup!(setup)
  Array(setup).each do |u|
    user = User.create!(name: u["email"], email: u["email"], password: "correct horse battery", verified: u["verified"])
    next unless u["account"]
    account = Account.create!(name: "#{u["email"]} co")
    account.memberships.create!(user: user, role: "owner")
    account.applications.create!(name: "Shop")
  end
end

CASES.each do |c|
  ActiveRecord::Base.transaction(requires_new: true) do
    setup!(c["setup"])
    s = session
    out = case c["kind"]
    when "flow"
      mock_next(c["next"])
      status, back = click_google(s)
      status == 302 ? land(s, path_of(back)) : {provider_status: status}
    when "state_mismatch"
      mock_next(c["next"])
      _, back = click_google(s)
      land(s, path_of(back).sub("state=", "state=0"))
    when "replay"
      mock_next(c["next"])
      token = csrf(s)
      s.post "/auth/google_oauth2", params: {authenticity_token: token}
      jar = s.cookies.to_hash
      back = Net::HTTP.get_response(URI(s.response.location))["location"]
      first = land(s, path_of(back))
      thief = session
      jar.each { |k, v| thief.cookies[k] = v unless k == "session_token" }
      {genuine: first, replay: land(thief, path_of(back))}
    when "access_denied"
      mock_next(c["next"])
      _, back = click_google(s)
      st = back[/state=([^&]+)/, 1]
      land(s, "/auth/google_oauth2/callback?error=access_denied&state=#{st}")
    when "bad_csrf"
      csrf(s)
      begin
        s.post "/auth/google_oauth2", params: {authenticity_token: "forged"}
        loc = s.response.location && path_of(s.response.location)
        loc ? land(s, loc).merge(request_phase: loc) : {status: s.response.status}
      rescue => e
        {raised: e.class.name}
      end
    end
    puts JSON.generate({case: c["name"], out: out, users: users})
    raise ActiveRecord::Rollback
  end
end
