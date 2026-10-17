# Renders WeeklyDigestJob's mails under travel_to(NOW) without delivering
# them: WeeklyDigestJob#perform's loop, with DigestMailer...weekly built
# instead of deliver_later'd. Writes /tmp/digest-rails.json:
# [{account_id, to, subject, text, html}] in delivery order. The
# server's base URL is replaced by URL_PLACEHOLDER (the only normalisation).
require "active_support/testing/time_helpers"
include ActiveSupport::Testing::TimeHelpers

now = Time.at(Integer(ENV.fetch("NOW"))).utc
base = Rails.application.routes.url_helpers.root_url(**ActionMailer::Base.default_url_options).chomp("/")
placeholder = ENV.fetch("URL_PLACEHOLDER")
out = []
travel_to(now) do
  job = WeeklyDigestJob.new
  since = 7.days.ago
  Account.includes(:users).find_each do |account|
    recipients = account.users.where(weekly_digest: true)
    next if recipients.empty?
    digest = job.build(account, since)
    next if digest[:environments].empty?
    recipients.each do |user|
      mail = DigestMailer.with(user: user, account: account, digest: digest).weekly
      out << {account_id: account.id, to: mail.to, subject: mail.subject,
              text: mail.text_part.body.decoded.gsub(base, placeholder),
              html: mail.html_part.body.decoded.gsub(base, placeholder)}
    end
  end
end
File.write("/tmp/digest-rails.json", JSON.pretty_generate(out))
puts "CAPTURED #{out.size} mails"
