//! Fixed vocabularies the detectors match against: probe paths, user-agent
//! families, routes that are not browser pages, privilege halts, suspicious
//! OAuth client names and a small disposable-email list. Code, not config:
//! changing one changes what every application raises, so it goes through
//! review like any detector change.

/// Railwatch's own probe fragments (`probe:true`) plus framework debug
/// pages and credential files.
pub fn probe(path: &str) -> bool {
    const EXTRA: [&str; 4] = ["laravel.log", "/_ignition", "/telescope", "credentials.json"];
    let p = path.to_ascii_lowercase();
    crate::dashboard::filter_query::PROBE_PATTERNS.iter().chain(EXTRA.iter()).any(|f| p.contains(f))
}

/// A user agent's family. `app` is the application's own client prefixes
/// (a mobile app's `ShopMobile/1.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UaFamily {
    None,
    App,
    Curl,
    Python,
    Script,
    Bot,
    /// `Mozilla/5.0` and nothing else: no engine token, a script pretending.
    BareMozilla,
    Browser,
    Other,
}

impl UaFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            UaFamily::None => "none",
            UaFamily::App => "app",
            UaFamily::Curl => "curl",
            UaFamily::Python => "python",
            UaFamily::Script => "script",
            UaFamily::Bot => "bot",
            UaFamily::BareMozilla => "bare-mozilla",
            UaFamily::Browser => "browser",
            UaFamily::Other => "other",
        }
    }

    /// Sends requests no person's browser would.
    pub fn non_browser(self) -> bool {
        matches!(self, UaFamily::Curl | UaFamily::Python | UaFamily::Script | UaFamily::BareMozilla)
    }

    /// A script's family (what a spoofed app client also sends).
    pub fn scripted(self) -> bool {
        matches!(self, UaFamily::Curl | UaFamily::Python | UaFamily::Script)
    }
}

pub fn ua_family(ua: Option<&str>, app_prefixes: &[String]) -> UaFamily {
    let Some(ua) = ua.map(str::trim).filter(|u| !u.is_empty()) else { return UaFamily::None };
    if app_prefixes.iter().any(|p| ua.starts_with(p.as_str())) {
        return UaFamily::App;
    }
    let l = ua.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    if has(&["curl", "wget", "httpie"]) {
        UaFamily::Curl
    } else if has(&["python", "aiohttp", "httpx", "requests/"]) {
        UaFamily::Python
    } else if has(&["go-http", "okhttp", "java/", "libwww", "node-fetch", "axios", "undici"]) {
        UaFamily::Script
    } else if has(&["bot", "crawl", "spider", "scan"]) {
        UaFamily::Bot
    } else if ua == "Mozilla/5.0" {
        UaFamily::BareMozilla
    } else if ua.starts_with("Mozilla/5.0 ") && has(&["applewebkit", "gecko", "trident"]) {
        UaFamily::Browser
    } else {
        UaFamily::Other
    }
}

/// Machine endpoints, assets and health checks: not pages a browser asks
/// for, so a script on them is expected.
pub fn browser_route(path: &str) -> bool {
    const NOT_WEB: [&str; 14] = [
        "/api/",
        "/mcp",
        "/.well-known",
        "/vite",
        "/assets/",
        "/packs/",
        "/cable",
        "/railwatch",
        "/lantern",
        "/rails/",
        "/manifest",
        "/oauth/",
        "/ingest",
        "/webhooks/",
    ];
    let p = path.to_ascii_lowercase();
    !(p == "/up" || p == "/health" || p.starts_with("/up?") || NOT_WEB.iter().any(|n| p.starts_with(n)))
}

/// before_actions that refuse a privilege the caller lacks.
pub fn privilege_halt(halted: &str) -> bool {
    let h = halted.to_ascii_lowercase();
    h.starts_with("require_") && (h.contains("admin") || h.contains("superadmin") || h.contains("staff") || h.contains("owner"))
        || h == "set_current_organization"
        || h == "set_current_account"
        || h == "authorize_admin"
}

/// An OAuth client name that announces what it is for.
pub fn suspicious_client_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["pentest", "probe", "exploit", "burp", "hack", "attack", "scanner", "sqlmap", "nuclei"].iter().any(|w| n.contains(w)) || n.trim() == "test"
}

/// A redirect URI on a reserved name (example.invalid, .test, localhost on
/// a public client): registered by someone who never meant to use it.
pub fn throwaway_redirect(uri: &str) -> bool {
    let host = uri.split("://").nth(1).unwrap_or(uri).split(['/', '?', '#']).next().unwrap_or("").rsplit('@').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h).to_ascii_lowercase();
    host.ends_with(".invalid") || host.ends_with(".test") || host.ends_with(".example") || host == "example.com" || host.ends_with(".example.com")
}

/// A short list of disposable-email providers. The application knows more
/// (and may send `disposable: true` on its event, which wins); this is the
/// floor for an app that sends only the domain.
pub fn disposable_domain(domain: &str) -> bool {
    const DOMAINS: [&str; 24] = [
        "mailinator.com",
        "guerrillamail.com",
        "guerrillamail.net",
        "sharklasers.com",
        "10minutemail.com",
        "tempmail.com",
        "temp-mail.org",
        "throwawaymail.com",
        "yopmail.com",
        "getnada.com",
        "trashmail.com",
        "dispostable.com",
        "maildrop.cc",
        "mintemail.com",
        "fakeinbox.com",
        "mohmal.com",
        "emailondeck.com",
        "spamgourmet.com",
        "mailnesia.com",
        "tempail.com",
        "moakt.com",
        "burnermail.io",
        "inboxkitten.com",
        "mail.tm",
    ];
    let d = domain.trim().trim_start_matches('@').to_ascii_lowercase();
    DOMAINS.iter().any(|x| d == *x || d.ends_with(&format!(".{x}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families() {
        let app = vec!["ShopMobile/".to_string()];
        for (ua, f) in [
            (None, UaFamily::None),
            (Some("curl/8.4.0"), UaFamily::Curl),
            (Some("python-requests/2.31"), UaFamily::Python),
            (Some("Go-http-client/1.1"), UaFamily::Script),
            (Some("Mozilla/5.0"), UaFamily::BareMozilla),
            (Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15"), UaFamily::Browser),
            (Some("ShopMobile/1.0 (iPhone)"), UaFamily::App),
            (Some("Googlebot/2.1"), UaFamily::Bot),
        ] {
            assert_eq!(ua_family(ua, &app), f, "{ua:?}");
        }
    }

    #[test]
    fn vocabularies() {
        assert!(probe("/.env.local") && probe("/storage/logs/laravel.log") && probe("/x/.GIT/config") && !probe("/orders/1"));
        assert!(browser_route("/admin/console") && browser_route("/") && !browser_route("/api/v1/x") && !browser_route("/up") && !browser_route("/mcp"));
        assert!(privilege_halt("require_superadmin") && privilege_halt("require_admin") && privilege_halt("set_current_organization"));
        assert!(!privilege_halt("authenticate") && !privilege_halt("require_login"));
        assert!(suspicious_client_name("pentest-tool") && suspicious_client_name("Test") && !suspicious_client_name("Claude"));
        assert!(
            throwaway_redirect("https://example.invalid/cb")
                && throwaway_redirect("http://x.test:8080/cb")
                && !throwaway_redirect("https://claude.ai/api/mcp/auth_callback")
        );
        assert!(disposable_domain("Mailinator.com") && disposable_domain("eu.mailinator.com") && !disposable_domain("example.com"));
    }
}
