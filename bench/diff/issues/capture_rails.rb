# Runs inside rw-baseline (bin/rails runner /tmp/issues_capture.rb).
# GETs account-level issue pages as an Inertia XHR, signed in as the
# props-diff user (an owner of the bench account), and writes
# {path => {component, props}} to /tmp/issues-rails.json. Paths come from
# /tmp/issues-paths.json. JSON endpoints (merge_candidates) are stored as
# {"json" => body}. Real clock: the Rust side is fetched right after.
user = User.find_by!(email: "props-diff@example.com")
paths = JSON.parse(File.read("/tmp/issues-paths.json"))
session_row = user.sessions.create!
s = ActionDispatch::Integration::Session.new(Rails.application)
s.host! "localhost"
jar = ActionDispatch::Request.new(Rails.application.env_config.merge("HTTP_HOST" => "localhost")).cookie_jar
jar.signed[:session_token] = session_row.id
jar.signed[:account_id] = Account.find_by!(slug: "bench").id
cookie = "session_token=#{CGI.escape(jar[:session_token])}; account_id=#{CGI.escape(jar[:account_id])}"
version = InertiaRails.configuration.version
version = version.call if version.respond_to?(:call)
out = {}
paths.each do |path|
  json = path.include?("merge_candidates")
  headers = {"Cookie" => cookie, "User-Agent" => "Mozilla/5.0 Chrome/140.0"}
  headers.merge!("X-Inertia" => "true", "X-Inertia-Version" => version.to_s, "Accept" => "text/html, application/xhtml+xml") unless json
  headers["Accept"] = "application/json" if json
  s.get path, headers: headers
  out[path] = if s.response.status != 200
    {"status" => s.response.status, "body" => s.response.body.to_s.first(300)}
  elsif json
    {"json" => JSON.parse(s.response.body)}
  else
    page = JSON.parse(s.response.body)
    {"component" => page["component"], "props" => page["props"]}
  end
end
File.write("/tmp/issues-rails.json", JSON.generate(out))
puts "CAPTURED #{out.size} (#{out.count { |_, v| v['status'] }} non-200) user=#{user.id} session=#{session_row.id}"
