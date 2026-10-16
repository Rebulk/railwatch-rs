# The import-rails proof (bench/install/ar_import_proof.sh): the Rails app,
# with its own configured Active Record encryption keys, saves integrations
# in a scratch account, snapshots the primary database (SQLite online
# backup) to RW_AR_SNAPSHOT, then destroys the scratch account again.
# Prints the keys import-rails needs as KEY=VALUE lines.
require "json"
snapshot = ENV.fetch("RW_AR_SNAPSHOT", "/tmp/ar_import_primary.sqlite3")
File.delete(snapshot) if File.exist?(snapshot)
account = Account.create!(name: "ar-import-proof #{SecureRandom.hex(3)}", slug: "ar-proof-#{SecureRandom.hex(4)}")
begin
  long = "https://hooks.slack.com/services/T000/B000/" + "y" * 180
  [
    ["webhook", "Signed webhook", {"url" => "https://hooks.example.com/railwatch", "secret" => "whsec_ünïcode"}],
    ["slack", "Slack (compressed)", {"webhook_url" => long}],
    ["email", "Email", {"to" => "ops@example.com"}]
  ].each do |kind, name, settings|
    i = account.integrations.new(kind: kind, name: name)
    i.settings = settings
    i.save!(validate: false)
  end
  raw = Integration.where(account: account).pluck(:id, :kind, :encrypted_settings)
  raw.each { |id, kind, enc| puts "RAILS_ROW #{id} #{kind} #{enc.to_s[0, 60]}" }
  conn = ActiveRecord::Base.connection.raw_connection
  dst = SQLite3::Database.new(snapshot)
  b = SQLite3::Backup.new(dst, "main", conn, "main"); b.step(-1); b.finish; dst.close
  expected = Integration.where(account: account).order(:id).map { |i| [i.id, i.settings] }
  File.write(snapshot + ".expected.json", JSON.generate(expected))
  c = ActiveRecord::Encryption.config
  puts "AR_PRIMARY_KEY=#{Array(c.primary_key).last}"
  puts "AR_SALT=#{c.key_derivation_salt}"
  puts "SNAPSHOT=#{snapshot}"
ensure
  Integration.where(account: account).delete_all
  account.delete
end
