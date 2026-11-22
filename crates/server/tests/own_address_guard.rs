//! Audit 2026-11-22 (rw-h-ssrf): the address rule for alert webhooks and
//! uptime monitors decided from fixed reserved ranges only, so this
//! server's own public address passed. A public address configured on one
//! of the host's interfaces is delivered locally, which reaches services a
//! firewall outside the machine hides.
//!
//! The sandbox and CI have only loopback, so the host's address is given as
//! data, through `RAILWATCH_URL` (an IP literal there is the server's own
//! address). Nothing is contacted: both checks only parse the URL.

use railwatch_server::alerts::outbound_http::{self, Kind};
use railwatch_server::uptime::probe;

/// A public address, standing for this server's own (never contacted).
const OWN: &str = "100.128.0.9";

#[test]
fn a_monitor_or_webhook_aimed_at_this_servers_own_address_is_refused_unless_private_targets_are_allowed() {
    unsafe { std::env::set_var("RAILWATCH_URL", format!("https://{OWN}")) };
    let lan = (probe::Policy { allow_private: true }, outbound_http::Policy { allow_private: true });
    let (monitor, hook) = (format!("http://{OWN}:8080/admin"), format!("https://{OWN}/hook"));

    assert!(probe::validate_url("http://100.128.0.10/up", probe::Policy::default()).is_ok(), "control: another public address is a valid monitor");
    assert!(outbound_http::validate_with("https://100.128.0.10/h", Kind::Webhook, outbound_http::Policy::default()).is_ok(), "control: and a valid webhook");
    assert!(probe::validate_url(&monitor, lan.0).is_ok(), "the self-host flag still allows this server's own address");
    assert!(outbound_http::validate_with(&hook, Kind::Webhook, lan.1).is_ok(), "the alert flag still allows it");

    assert!(probe::validate_url(&monitor, probe::Policy::default()).is_err(), "a monitor aimed at this server's own address was accepted");
    assert!(outbound_http::validate_with(&hook, Kind::Webhook, outbound_http::Policy::default()).is_err(), "a webhook aimed at this server's own address was accepted");
}
