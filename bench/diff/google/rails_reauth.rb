# frozen_string_literal: true

# Does railwatch-cloud's "Confirm with Google" button reach
# SessionsOauthController's reauthentication branch? Same setup as
# rails_flows.rb. Two shapes:
# - the old button (464f0fd): `reauthenticate` as a form field. OmniAuth
#   keeps only request.GET, so on any Rails it answers "You are already
#   signed in" with recently_authenticated: false.
# - the fixed button (2026-10-16): POST /auth/google_oauth2?reauthenticate=user.
#   Expected (not run yet): first "/settings/profile", notice "Identity
#   confirmed. You can now continue.", recently_authenticated: true. The
#   old controller already reads omniauth.params in the callback, so this
#   should hold on the old rw-baseline image too; the failure-path fix
#   needs the rebuilt image.
load "/tmp/rails_flows_lib.rb"

ActiveRecord::Base.transaction(requires_new: true) do
  s = session
  mock_next({"email" => "owner@example.com", "sub" => "g-owner"})
  _, back = click_google(s)
  land(s, path_of(back))
  Session.update_all(authenticated_at: 1.hour.ago)

  mock_next({"email" => "owner@example.com", "sub" => "g-owner"})
  s.get "/settings/profile"
  token = s.response.body[/name="csrf-token" content="([^"]+)"/, 1]
  # Exactly what google-sign-in-button.tsx submits with reauthenticateFor="user".
  s.post "/auth/google_oauth2", params: {authenticity_token: token, reauthenticate: "user", prompt: "select_account"}
  back = Net::HTTP.get_response(URI(s.response.location))["location"]
  out = land(s, path_of(back))
  puts JSON.generate({case: "reauthenticate (form field, as the button sends it)", out: out,
    recently_authenticated: Session.last.recently_authenticated?})
  raise ActiveRecord::Rollback
end

ActiveRecord::Base.transaction(requires_new: true) do
  s = session
  mock_next({"email" => "owner@example.com", "sub" => "g-owner"})
  _, back = click_google(s)
  land(s, path_of(back))
  Session.update_all(authenticated_at: 1.hour.ago)

  mock_next({"email" => "owner@example.com", "sub" => "g-owner"})
  s.get "/settings/profile"
  token = s.response.body[/name="csrf-token" content="([^"]+)"/, 1]
  # What the fixed google-sign-in-button.tsx submits with reauthenticateFor="user".
  s.post "/auth/google_oauth2?reauthenticate=user", params: {authenticity_token: token, prompt: "select_account"}
  back = Net::HTTP.get_response(URI(s.response.location))["location"]
  out = land(s, path_of(back))
  puts JSON.generate({case: "reauthenticate (query string, as the fixed button sends it)", out: out,
    recently_authenticated: Session.last.recently_authenticated?})
  raise ActiveRecord::Rollback
end
