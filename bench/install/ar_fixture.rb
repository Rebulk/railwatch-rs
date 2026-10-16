# Writes ciphertexts made by this Rails' own Active Record encryption, for
# railwatch-rs import-rails to decrypt. Persists nothing: values are
# serialized through Integration's attribute type, never saved.
# ARGV-free: RW_AR_MODE=test uses fixed test keys (for the committed unit
# fixture); otherwise the container's configured keys are used.
require "json"
enc = ActiveRecord::Encryption
if ENV["RW_AR_MODE"] == "test"
  enc.config.key_derivation_salt = "railwatch-rs test salt"
  enc.config.primary_key = ["old test primary key", "current test primary key"]
  enc.instance_variable_set(:@key_provider, nil) rescue nil
end
provider = ActiveRecord::Encryption::DerivedSecretKeyProvider.new(Array(enc.config.primary_key))
type = Integration.type_for_attribute(:encrypted_settings)
rows = [
  {"kind" => "webhook", "settings" => {"url" => "https://hooks.example.com/rw?x=1", "secret" => "s3cr3t-✓"}},
  {"kind" => "slack", "settings" => {"webhook_url" => "https://hooks.slack.com/services/T000/B000/" + "x" * 200}},
  {"kind" => "linear", "settings" => {"access_token" => "lin_oauth_abc", "refresh_token" => "lin_ref_def"}}
]
out = rows.map do |r|
  ct = ActiveRecord::Encryption.with_encryption_context(key_provider: provider) { type.serialize(r["settings"]) }
  back = ActiveRecord::Encryption.with_encryption_context(key_provider: provider) { type.deserialize(ct) }
  raise "round trip" unless back == r["settings"]
  r.merge("ciphertext" => ct, "compressed" => JSON.parse(ct)["h"].key?("c"))
end
# One encrypted with the OLD key only (a key rotation): must still decrypt.
old = ActiveRecord::Encryption::DerivedSecretKeyProvider.new([Array(enc.config.primary_key).first])
ct = ActiveRecord::Encryption.with_encryption_context(key_provider: old) { type.serialize({"url" => "https://old.example.com", "secret" => "old"}) }
out << {"kind" => "webhook", "settings" => {"url" => "https://old.example.com", "secret" => "old"}, "ciphertext" => ct, "compressed" => false}
File.write(ENV.fetch("RW_AR_OUT", "/tmp/ar_fixture.json"), JSON.pretty_generate(
  "rails" => Rails.version,
  "hash_digest_class" => enc.config.hash_digest_class.to_s,
  "rows" => out
))
puts "wrote #{out.size} rows"
