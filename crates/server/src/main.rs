//! `railwatch` -- the server binary. Configuration: env vars with a
//! railwatch.toml fallback (config.rs).
//!
//!   railwatch serve
//!   railwatch setup [--email E --password-stdin --name N --account A --app APP --env ENV]
//!                   [--alert-webhook URL [--alert-webhook-secret S] | --alert-slack URL | --alert-email ADDR]
//!   railwatch env list | create <slug> [--retention-days N] | import <slug> <token_digest> [--retention-days N]
//!   railwatch import-rails --primary <production.sqlite3> --telemetry-dir <dir>
//!                          [--ar-primary-key K]... [--ar-key-derivation-salt S]
//!   railwatch split <slug>|--all       single tenant file -> per-day directory
//!   railwatch export <slug> <out.sqlite3>   per-day directory -> one Rails file
//!   railwatch backup                   one backup pass now (RAILWATCH_BACKUP_S3_*)
//!   railwatch restore                  download the backup into an empty data dir
//!   railwatch integrations [--show-secrets]   one JSON line per alert destination
//!   railwatch integrations test <id>   Send test: one synthetic alert to that destination
//!   railwatch digest send --dry-run [--account ID]   the weekly digests a run now would send
//!   railwatch doctor
//!   railwatch upgrade [--version vX.Y.Z] [--allow-downgrade]   fetch the release's install.sh and re-run it
//!   railwatch version

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState};

const USAGE: &str = "usage:
  railwatch serve
  railwatch setup [--email E] [--password-stdin] [--name NAME] [--account NAME] [--app NAME] [--env NAME]
                  [--alert-webhook URL [--alert-webhook-secret S]] [--alert-slack URL] [--alert-email ADDR]
  railwatch env list
  railwatch env create <slug> [--retention-days N]
  railwatch env import <slug> <token_digest> [--retention-days N]
  railwatch import-rails --primary <production.sqlite3> --telemetry-dir <dir>
                         [--ar-primary-key K]... [--ar-key-derivation-salt S]
  railwatch split <slug> | --all
  railwatch export <slug> <out.sqlite3>
  railwatch backup
  railwatch restore
  railwatch integrations [--show-secrets]
  railwatch integrations test <id>
  railwatch digest send --dry-run [--account ID]   print the weekly digests a run now would send
  railwatch doctor
  railwatch upgrade [--version vX.Y.Z] [--allow-downgrade]
                                         as root: install.sh from the release, re-run (zero-drop restart);
                                         an older release only with --allow-downgrade
  railwatch version

Configuration: RAILWATCH_* env vars (RAILWATCH_ENV_FILE=PATH reads them from a KEY=VALUE file),
else railwatch.toml (RAILWATCH_CONFIG to point elsewhere). Run as root, every command but
serve and upgrade switches to the owner of the data dir first.
See `railwatch doctor` for every setting and where it came from.";

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("railwatch: {msg}");
    ExitCode::FAILURE
}

fn config() -> Config {
    Config::load().unwrap_or_else(|e| {
        eprintln!("railwatch: config: {e}");
        std::process::exit(2)
    })
}

fn open_control(cfg: &Config) -> Arc<Control> {
    std::fs::create_dir_all(&cfg.data_dir).unwrap_or_else(|e| {
        eprintln!("railwatch: cannot create {}: {e}", cfg.data_dir.display());
        std::process::exit(1)
    });
    Arc::new(Control::open_with(&cfg.control_path(), cfg.enforce_quota).unwrap_or_else(|e| {
        eprintln!("railwatch: cannot open {}: {e}", cfg.control_path().display());
        std::process::exit(1)
    }))
}

/// The data directory's instance lock: one `railwatch serve` at a time.
/// The tenant writers cache their high-water marks (storage/writer.rs) and
/// the scheduler, watchdog and alert loop assume they are alone, so a second
/// server on the same data must not start writing until the first has
/// drained and exited. `flock` on `<data_dir>/serve.lock`, held for the
/// life of the process: the kernel releases it on exit, crash included, and
/// it works across containers sharing one volume.
///
/// `wait`: block until the holder exits (serve; this is the hand-over of a
/// restart that starts the next server before stopping the last). Otherwise
/// fail at once (split, import-rails, restore: offline work).
fn lock_data_dir(cfg: &Config, wait: bool) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| format!("cannot create {}: {e}", cfg.data_dir.display()))?;
    let path = cfg.data_dir.join("serve.lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on an fd we own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(file);
    }
    if !wait {
        return Err(format!("{} is held: a railwatch server is running on {} (stop it first)", path.display(), cfg.data_dir.display()));
    }
    eprintln!("railwatch: waiting for {}: another railwatch server on this data dir has not exited yet", path.display());
    let started = std::time::Instant::now();
    // SAFETY: as above; blocks until the holder's fd closes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!("{}: {}", path.display(), std::io::Error::last_os_error()));
    }
    eprintln!("railwatch: {} acquired after {} ms", path.display(), started.elapsed().as_millis());
    Ok(file)
}

/// serve's lock. With `standby_relay`, when another server holds it and
/// this one can bind its own address (it is in another network namespace:
/// a second container on the same volume), the address is served at once,
/// relayed to the holder through serve.sock, until the lock is ours
/// (standby.rs); the listener is returned with the lock. Otherwise (no
/// relay, an inherited socket, TLS, the address taken by the holder
/// itself) this waits for the lock as before.
fn acquire_data_dir(cfg: &Config, runtime: &tokio::runtime::Runtime) -> Result<(std::fs::File, Option<tokio::net::TcpListener>), String> {
    let relayable = cfg.standby_relay && cfg.tls.is_none() && std::env::var_os("LISTEN_FDS").is_none();
    if !relayable {
        return lock_data_dir(cfg, true).map(|l| (l, None));
    }
    if let Ok(l) = lock_data_dir(cfg, false) {
        return Ok((l, None));
    }
    let Some(socket) = railwatch_server::standby::socket_path(&cfg.data_dir) else {
        eprintln!("railwatch: standby_relay: {} is too long for a socket path; waiting without relaying", cfg.data_dir.display());
        return lock_data_dir(cfg, true).map(|l| (l, None));
    };
    let listener = match std::net::TcpListener::bind(&cfg.bind) {
        Ok(l) => l,
        Err(e) => {
            // The holder has this address: same host and network, so every
            // client already reaches it.
            eprintln!("railwatch: standby: cannot bind {} ({e}); the server holding the data dir answers there", cfg.bind);
            return lock_data_dir(cfg, true).map(|l| (l, None));
        }
    };
    listener.set_nonblocking(true).map_err(|e| format!("{}: {e}", cfg.bind))?;
    eprintln!(
        "railwatch: standby: another server holds {}/serve.lock; relaying {} to it through {} until it exits",
        cfg.data_dir.display(),
        cfg.bind,
        socket.display()
    );
    let wait_cfg = cfg.clone();
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener).map_err(|e| format!("{}: {e}", wait_cfg.bind))?;
        let lock_cfg = wait_cfg.clone();
        let held = tokio::task::spawn_blocking(move || lock_data_dir(&lock_cfg, true));
        let (listener, held) = railwatch_server::standby::relay(listener, socket, wait_cfg.http.max_connections, held).await;
        let lock = held.map_err(|e| format!("waiting for serve.lock: {e}"))??;
        Ok((lock, Some(listener)))
    })
}

/// Runs an offline command only while no server holds the data dir.
fn offline(f: impl FnOnce() -> ExitCode) -> ExitCode {
    match lock_data_dir(&config(), false) {
        Ok(_lock) => f(),
        Err(e) => fail(e),
    }
}

fn tenants(cfg: &Config) -> Tenants {
    let (tx, _rx) = followups::channel();
    Tenants::new(cfg.tenants_dir(), Duration::from_secs(1), tx)
}

fn main() -> ExitCode {
    railwatch_server::memory::configure();
    if let Err(e) = railwatch_server::config::load_env_file() {
        return fail(format!("RAILWATCH_ENV_FILE: {e}"));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if !matches!(args.first().copied(), None | Some("serve" | "upgrade" | "version" | "--version" | "-V" | "help" | "--help" | "-h"))
        && let Err(e) = drop_to_data_dir_owner()
    {
        return fail(e);
    }
    match args.as_slice() {
        ["serve"] | [] => serve(),
        ["setup", rest @ ..] => setup(rest),
        ["doctor"] => doctor(),
        ["integrations"] => integrations(false),
        ["integrations", "--show-secrets"] => integrations(true),
        ["integrations", "test", id] => integration_test(id),
        ["digest", "send", "--dry-run"] => digest_dry_run(None),
        ["digest", "send", "--dry-run", "--account", id] | ["digest", "send", "--account", id, "--dry-run"] => match id.parse() {
            Ok(id) => digest_dry_run(Some(id)),
            Err(_) => usage(),
        },
        ["import-rails", rest @ ..] => offline(|| import_rails(rest)),
        ["split", "--all"] => offline(split_all),
        ["split", slug] => offline(|| split(slug)),
        ["export", slug, out] => export(slug, out),
        ["backup"] => backup_now(),
        ["restore"] => offline(restore),
        ["env", "list"] => {
            let cfg = config();
            for e in open_control(&cfg).list().expect("list environments") {
                println!("{}\t{}\tretention={}d\tpaused={}\tevents_this_month={}", e.id, e.slug, e.retention_days, e.paused, e.events_this_month);
            }
            ExitCode::SUCCESS
        }
        ["env", "create", slug, rest @ ..] => {
            let Some(retention) = retention(rest) else { return usage() };
            let token = control::new_token();
            match create_env(slug, &control::digest(&token), retention) {
                Ok(()) => {
                    println!("{token}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        ["env", "import", slug, digest, rest @ ..] => {
            let Some(retention) = retention(rest) else { return usage() };
            if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return fail("token digest must be 64 hex characters (SHA-256 of the token)");
            }
            match create_env(slug, &digest.to_ascii_lowercase(), retention) {
                Ok(()) => {
                    println!("imported {slug}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        ["version" | "--version" | "-V"] => {
            println!("railwatch {}", railwatch_server::VERSION);
            ExitCode::SUCCESS
        }
        ["upgrade", rest @ ..] => upgrade(rest),
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

fn retention(rest: &[&str]) -> Option<i64> {
    match rest {
        [] => Some(30),
        ["--retention-days", n] => n.parse().ok().filter(|n| *n > 0),
        _ => None,
    }
}

fn create_env(slug: &str, digest: &str, retention: i64) -> Result<(), String> {
    let cfg = config();
    let control = open_control(&cfg);
    control.create(slug, digest, retention).map_err(|e| e.to_string())?;
    // Environment's after_create_commit: the tenant file exists from the start.
    tenants(&cfg).ensure(slug).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// split / export (storage::export)

/// Splits one environment's single file into per-day files. Run with the
/// server stopped; the old file is kept as `<slug>.sqlite3.split` until the
/// operator deletes it.
fn split(slug: &str) -> ExitCode {
    let cfg = config();
    let dir = cfg.tenants_dir();
    let file = dir.join(format!("{slug}.sqlite3"));
    if !file.is_file() {
        return fail(format!("{} is not a file", file.display()));
    }
    let started = std::time::Instant::now();
    match railwatch_server::storage::export::split(&file, &dir.join(slug)) {
        Ok(m) => {
            let kept = dir.join(format!("{slug}.sqlite3.split"));
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::rename(format!("{}{suffix}", file.display()), format!("{}{suffix}", kept.display()));
            }
            println!(
                "{slug}: {} rows into {} files, {} -> {} bytes, {:.1} s; old file kept at {}",
                m.rows,
                m.files,
                m.bytes_in,
                m.bytes_out,
                started.elapsed().as_secs_f64(),
                kept.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("{slug}: {e}")),
    }
}

fn split_all() -> ExitCode {
    let cfg = config();
    let control = open_control(&cfg);
    let mut status = ExitCode::SUCCESS;
    for e in control.list().expect("list environments") {
        if cfg.tenants_dir().join(format!("{}.sqlite3", e.slug)).is_file() && split(&e.slug) != ExitCode::SUCCESS {
            status = ExitCode::FAILURE;
        }
    }
    status
}

fn export(slug: &str, out: &str) -> ExitCode {
    let cfg = config();
    let dir = cfg.tenants_dir().join(slug);
    if !railwatch_server::storage::layout::meta_path(&dir).is_file() {
        return fail(format!("{} is not a per-day tenant", dir.display()));
    }
    let started = std::time::Instant::now();
    match railwatch_server::storage::export::export(&dir, std::path::Path::new(out)) {
        Ok(m) => {
            println!("{slug}: {} rows from {} files into {out} ({} bytes), {:.1} s", m.rows, m.files, m.bytes_out, started.elapsed().as_secs_f64());
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("{slug}: {e}")),
    }
}

fn backup_settings(cfg: &Config) -> Option<railwatch_server::storage::backup::Settings> {
    if cfg.backup.is_none() {
        eprintln!("railwatch: backups are off; set RAILWATCH_BACKUP_S3_ENDPOINT, _BUCKET, _ACCESS_KEY_ID, _SECRET_ACCESS_KEY");
    }
    cfg.backup.clone()
}

fn backup_now() -> ExitCode {
    let cfg = config();
    let Some(b) = backup_settings(&cfg) else { return ExitCode::from(2) };
    let started = std::time::Instant::now();
    match railwatch_server::storage::backup::run_pass(&b.s3, &cfg.data_dir, &cfg.tenants_dir(), railwatch_server::time::now_micros()) {
        Ok(p) => {
            railwatch_server::storage::backup::record_ok(&cfg.data_dir);
            println!(
                "{} snapshots, {} day files uploaded, {} already there; {} -> {} bytes, {:.1} s",
                p.snapshots,
                p.day_files,
                p.skipped,
                p.bytes_raw,
                p.bytes_compressed,
                started.elapsed().as_secs_f64()
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn restore() -> ExitCode {
    let cfg = config();
    let Some(b) = backup_settings(&cfg) else { return ExitCode::from(2) };
    let started = std::time::Instant::now();
    match railwatch_server::storage::backup::restore(&b.s3, &cfg.data_dir) {
        Ok(r) => {
            println!("restored {} files, {} bytes into {}, {:.1} s", r.files, r.bytes, cfg.data_dir.display(), started.elapsed().as_secs_f64());
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------------------
// setup

fn flag<'a>(rest: &[&'a str], name: &str) -> Option<&'a str> {
    rest.iter().position(|a| *a == name).and_then(|i| rest.get(i + 1).copied())
}

fn prompt(label: &str, default: Option<&str>) -> String {
    let stdin = std::io::stdin();
    loop {
        match default {
            Some(d) => eprint!("{label} [{d}]: "),
            None => eprint!("{label}: "),
        }
        std::io::stderr().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            eprintln!();
            std::process::exit(2);
        }
        let v = line.trim();
        match (v.is_empty(), default) {
            (false, _) => return v.to_string(),
            (true, Some(d)) => return d.to_string(),
            (true, None) => continue,
        }
    }
}

fn setup(rest: &[&str]) -> ExitCode {
    let known =
        ["--email", "--name", "--account", "--app", "--env", "--password-stdin", "--alert-webhook", "--alert-webhook-secret", "--alert-slack", "--alert-email"];
    if rest.iter().any(|a| a.starts_with("--") && !known.contains(a)) {
        return usage();
    }
    let cfg = config();
    // The alert destinations, validated before anything is written so a typo
    // fails the whole setup instead of leaving an install that never alerts.
    let alerts = match setup_alerts(rest, &cfg) {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    let control = open_control(&cfg);
    let users: i64 = control.with(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))).unwrap_or(0);
    if users > 0 {
        return fail(format!(
            "{} already has users; sign in at {}/sign_in (or `railwatch env create` for another token)",
            cfg.control_path().display(),
            cfg.url
        ));
    }
    let interactive = std::io::stdin().is_terminal();
    let email = flag(rest, "--email").map(str::to_string).unwrap_or_else(|| prompt("Admin email", None));
    let password = if rest.contains(&"--password-stdin") {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok();
        line.trim_end_matches(['\r', '\n']).to_string()
    } else if interactive {
        loop {
            let p = rpassword::prompt_password(format!("Password (at least {} characters): ", control::MIN_PASSWORD)).unwrap_or_default();
            let errors = control::password_errors(&p);
            if !errors.is_empty() {
                eprintln!("{}", errors.iter().map(|(k, m)| control::full_message(k, m)).collect::<Vec<_>>().join(", "));
                continue;
            }
            if rpassword::prompt_password("Confirm password: ").unwrap_or_default() == p {
                break p;
            }
            eprintln!("Passwords do not match.");
        }
    } else {
        return fail("no terminal: pass --password-stdin and the password on stdin");
    };
    let default_name = email.split('@').next().unwrap_or("Admin").to_string();
    let name = flag(rest, "--name").map(str::to_string).unwrap_or_else(|| if interactive { prompt("Your name", Some(&default_name)) } else { default_name });
    let account = flag(rest, "--account")
        .map(str::to_string)
        .unwrap_or_else(|| if interactive { prompt("Account name", Some("My company")) } else { "My company".into() });
    let app =
        flag(rest, "--app").map(str::to_string).unwrap_or_else(|| if interactive { prompt("First application", Some("My app")) } else { "My app".into() });
    let env =
        flag(rest, "--env").map(str::to_string).unwrap_or_else(|| if interactive { prompt("Environment", Some("production")) } else { "production".into() });

    let errors = control::validate_new_user(&name, &control::normalize_email(&email), &password, None);
    if !errors.is_empty() {
        return fail(errors.iter().map(|(k, m)| control::full_message(k, m)).collect::<Vec<_>>().join(", "));
    }
    let result = control.tx(|tx| {
        let user = control::insert_user(tx, &name, &email, &password, true)?;
        let account = control::create_account(tx, &account, user)?;
        let (slug, prefix) = control::application_defaults(&app, None);
        let app = control::insert_application(tx, account, app.trim(), &slug, &prefix, None, "main")?;
        let (env_id, token) = control::create_environment(tx, app, &env)?;
        let env_slug: String = tx.query_row("SELECT slug FROM environments WHERE id = ?", [env_id], |r| r.get(0))?;
        for (kind, name, settings) in &alerts.integrations {
            let id = railwatch_server::alerts::integration::insert(tx, &alerts.cipher, account, kind, name, settings)?;
            for event in SETUP_ALERT_EVENTS {
                railwatch_server::web::integrations::insert_rule(tx, app, id, event, &serde_json::Map::new())?;
            }
        }
        Ok((env_slug, token))
    });
    let (slug, token) = match result {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    if let Err(e) = tenants(&cfg).ensure(&slug) {
        return fail(format!("creating the telemetry file for {slug}: {e}"));
    }
    println!("Railwatch is set up. Sign in at {}/sign_in as {}.", cfg.pages_url(), control::normalize_email(&email));
    println!();
    println!("Add these to the Rails app's environment (the token is shown once):");
    println!();
    println!("  RAILWATCH_INGEST_URL={}", cfg.url);
    println!("  RAILWATCH_TOKEN={token}");
    println!();
    if alerts.integrations.is_empty() {
        println!("No alert destination yet: nothing will tell you when the app goes down. Add one at");
        println!("{}/settings/integrations, or rerun setup on a fresh data dir with --alert-webhook/--alert-slack/--alert-email.", cfg.pages_url());
    } else {
        for (kind, name, _) in &alerts.integrations {
            println!("Alerts: {name} ({kind}) gets {}.", SETUP_ALERT_EVENTS.join(", "));
        }
        println!(
            "Silence is noticed after {} s without data (RAILWATCH_SILENCE_SECONDS). Try it: Settings > Integrations > Send test.",
            railwatch_server::jobs::watchdog::silence_secs()
        );
    }
    let host = cfg.url.trim_start_matches("http://").split(['/', ':']).next().unwrap_or("");
    if cfg.url.starts_with("http://") && !["localhost", "127.0.0.1", "[::1]"].contains(&host) {
        println!();
        println!("  # Plain http to a non-local host: the gem refuses it unless allowed");
        println!("  RAILWATCH_ALLOW_HTTP=true");
    }
    ExitCode::SUCCESS
}

/// What `setup --alert-*` subscribes each destination to: new and
/// regressed issues, and the watchdog's "app went quiet / is back".
const SETUP_ALERT_EVENTS: [&str; 4] = ["new_issue", "regressed_issue", "silent_environment", "environment_recovered"];

struct SetupAlerts {
    cipher: railwatch_server::alerts::integration::SettingsCipher,
    integrations: Vec<(&'static str, String, serde_json::Map<String, serde_json::Value>)>,
}

fn setup_alerts(rest: &[&str], cfg: &Config) -> Result<SetupAlerts, String> {
    use railwatch_server::alerts::integration;
    use serde_json::{Map, Value};
    let mut out = Vec::new();
    let mut add = |kind: &'static str, name: &str, pairs: Vec<(&str, String)>| -> Result<(), String> {
        let settings: Map<String, Value> = pairs.into_iter().map(|(k, v)| (k.to_string(), Value::String(v))).collect();
        let errors = integration::validate(kind, name, &settings);
        if !errors.is_empty() {
            let msg = errors.iter().map(|(k, m)| control::full_message(k, m)).collect::<Vec<_>>().join(", ");
            let hint = if msg.contains("non-public") { " (set RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1 for a receiver on your own network)" } else { "" };
            return Err(format!("--alert-{kind}: {msg}{hint}"));
        }
        out.push((kind, name.to_string(), settings));
        Ok(())
    };
    let secret = flag(rest, "--alert-webhook-secret");
    if secret.is_some() && flag(rest, "--alert-webhook").is_none() {
        return Err("--alert-webhook-secret needs --alert-webhook".into());
    }
    if let Some(url) = flag(rest, "--alert-webhook") {
        let mut pairs = vec![("url", url.to_string())];
        if let Some(s) = secret {
            pairs.push(("secret", s.to_string()));
        }
        add("webhook", "Webhook", pairs)?;
    }
    if let Some(url) = flag(rest, "--alert-slack") {
        add("slack", "Slack", vec![("webhook_url", url.to_string())])?;
    }
    if let Some(addr) = flag(rest, "--alert-email") {
        if !control::valid_email(addr) {
            return Err(format!("--alert-email: {addr:?} is not an email address"));
        }
        if cfg.smtp.is_none() {
            eprintln!("warn: --alert-email without SMTP configured: email alerts will fail until smtp_address is set (see `railwatch doctor`)");
        }
        add("email", "Email", vec![("to", addr.to_string())])?;
    }
    let cipher = integration::SettingsCipher::new(&cfg.secret()?);
    Ok(SetupAlerts { cipher, integrations: out })
}

// ---------------------------------------------------------------------------
// import-rails

fn import_rails(rest: &[&str]) -> ExitCode {
    let (Some(primary), Some(dir)) = (flag(rest, "--primary"), flag(rest, "--telemetry-dir")) else { return usage() };
    let cfg = config();
    std::fs::create_dir_all(&cfg.data_dir).ok();
    let secrets = match import_secrets(rest, &cfg) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    match railwatch_server::import_rails::run(&cfg.control_path(), primary.as_ref(), dir.as_ref(), &cfg.tenants_dir(), secrets.as_ref()) {
        Ok(r) => {
            println!("imported into {}", cfg.control_path().display());
            for (t, n) in &r.tables {
                println!("  {t:28} {n:>8} rows");
            }
            for t in &r.skipped_tables {
                println!("  NOT IMPORTED: {t}");
            }
            for (t, c) in &r.skipped_columns {
                println!("  NOT IMPORTED: column {t}.{c} (not in the control schema)");
            }
            if r.secrets_converted > 0 {
                println!("integration secrets: {} decrypted with Rails' keys and re-encrypted with this install's secret_key_base", r.secrets_converted);
            }
            for (id, why) in &r.secrets_unreadable {
                println!("  UNREADABLE: integration {id}'s secrets ({why}); re-enter them in Settings > Integrations");
            }
            println!("tenant files: {} adopted into {}", r.tenants_found, r.tenants_dir.display());
            if !r.tenants_missing.is_empty() {
                let shown: Vec<&str> = r.tenants_missing.iter().take(5).map(String::as_str).collect();
                let more = r.tenants_missing.len().saturating_sub(shown.len());
                println!(
                    "  MISSING: {} environment(s) have no telemetry file in {dir} (one is created on first ingest): {}{}",
                    r.tenants_missing.len(),
                    shown.join(", "),
                    if more > 0 { format!(", and {more} more") } else { String::new() }
                );
            }
            println!("Everyone signs in again (Rails' cookies are not readable here); passwords and ingest tokens carry over.");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// Settings > Integrations > Send test, from the shell: one synthetic alert
/// through the real delivery path (alerts::deliver_test), recorded like the
/// button's. Safe beside a running server (the alert row is created already
/// claimed, so the server's delivery loop never sends it too).
fn integration_test(id: &str) -> ExitCode {
    let Ok(id) = id.parse::<i64>() else { return usage() };
    let cfg = config();
    let control = open_control(&cfg);
    match cfg.secret() {
        Ok(secret) => control.configure_alerting(railwatch_server::alerts::integration::Alerting {
            cipher: railwatch_server::alerts::integration::SettingsCipher::new(&secret),
            smtp: cfg.smtp.clone(),
            url: cfg.pages_url(),
            deliver: cfg.alert_delivery,
        }),
        Err(e) => return fail(format!("secret_key_base: {e}")),
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    match rt.block_on(railwatch_server::alerts::deliver_test(&control, id)) {
        Ok(outcome) => {
            println!("integration {id}: test alert {outcome}");
            if outcome == "sent" { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => fail(format!("integration {id}: test alert failed: {e}")),
    }
}

/// Every integration, one JSON object per line: whether its secrets are
/// readable with this install's key (after import-rails, the check that
/// nothing needs re-entering), and with --show-secrets the settings.
fn integrations(show_secrets: bool) -> ExitCode {
    use railwatch_server::alerts::integration::{self, SettingsCipher};
    let cfg = config();
    let cipher = match cfg.secret() {
        Ok(s) => SettingsCipher::new(&s),
        Err(e) => return fail(e),
    };
    let control = open_control(&cfg);
    let ids: Vec<i64> = match control.with(|c| c.prepare("SELECT id FROM integrations ORDER BY id")?.query_map([], |r| r.get(0))?.collect()) {
        Ok(ids) => ids,
        Err(e) => return fail(e),
    };
    let mut unreadable = 0;
    for id in ids {
        let Ok(Some(i)) = control.with(|c| integration::find(c, Some(&cipher), id)) else { continue };
        unreadable += usize::from(i.settings_error.is_some());
        let mut line =
            serde_json::json!({"id": i.id, "account_id": i.account_id, "kind": i.kind, "name": i.name, "enabled": i.enabled, "error": i.settings_error});
        if show_secrets {
            line["settings"] = serde_json::Value::Object(i.settings);
        }
        println!("{line}");
    }
    if unreadable > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// `railwatch digest send --dry-run`: the server sends the weekly digest
/// itself (Mondays 08:00 UTC); this prints what a run now would send.
fn digest_dry_run(account: Option<i64>) -> ExitCode {
    let cfg = config();
    let control = open_control(&cfg);
    let delivery = match (&cfg.smtp, cfg.alert_delivery) {
        (_, false) => "off (alert_delivery = off: digests are logged, never sent)",
        (None, true) => "off (SMTP is not configured: digests are logged, not sent)",
        (Some(_), true) => "on (SMTP)",
    };
    let now = railwatch_server::time::now_micros();
    match railwatch_server::jobs::weekly_digest::dry_run(&control, &cfg.tenants_dir(), &cfg.pages_url(), now, account, delivery) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => fail(format!("no account {}", account.unwrap_or_default())),
        Err(e) => fail(e),
    }
}

/// Rails' Active Record encryption keys, from `--ar-primary-key` (repeat
/// for rotated keys, oldest first) and `--ar-key-derivation-salt`, else the
/// env vars railwatch-cloud reads (ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY,
/// _KEY_DERIVATION_SALT, _PREVIOUS_KEYS). None: nothing given.
fn import_secrets(rest: &[&str], cfg: &Config) -> Result<Option<railwatch_server::import_rails::Secrets>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let mut keys: Vec<String> = rest.windows(2).filter(|w| w[0] == "--ar-primary-key").map(|w| w[1].to_string()).collect();
    if keys.is_empty() {
        keys.extend(env("ACTIVE_RECORD_ENCRYPTION_PREVIOUS_KEYS").into_iter().flat_map(|v| v.split(',').map(|k| k.trim().to_string()).collect::<Vec<_>>()));
        keys.extend(env("ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY"));
    }
    keys.retain(|k| !k.is_empty());
    let salt = flag(rest, "--ar-key-derivation-salt").map(str::to_string).or_else(|| env("ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT"));
    match (keys.is_empty(), salt) {
        (true, None) => Ok(None),
        (false, Some(salt)) => Ok(Some(railwatch_server::import_rails::Secrets {
            rails: railwatch_server::active_record_encryption::Keys::derive(&keys, &salt),
            cipher: railwatch_server::alerts::integration::SettingsCipher::new(&cfg.secret()?),
        })),
        _ => Err("Rails integration secrets need both a primary key (--ar-primary-key) and the key derivation salt (--ar-key-derivation-salt)".into()),
    }
}

// ---------------------------------------------------------------------------
// upgrade

/// `railwatch upgrade`: downloads the release's install.sh and runs it with
/// `--upgrade`, which is exactly a re-run of the installer (one code path:
/// download, checksum, atomic swap, `systemctl restart railwatch.service`
/// with the socket held). `RAILWATCH_RELEASE_BASE` points at another
/// release host (a mirror, or a local server in tests).
/// Run as root against a data dir another user owns (the installed layout:
/// /var/lib/railwatch belongs to `railwatch`), a command would leave
/// root-owned files there (the control DB's -wal and -shm, a backup's
/// scratch) that the service then cannot open. So a CLI command run as root
/// becomes that owner first, and says so. Before any thread starts.
fn drop_to_data_dir_owner() -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let cfg = config();
    let Ok(meta) = std::fs::metadata(&cfg.data_dir) else { return Ok(()) };
    let (uid, gid) = (meta.uid(), meta.gid());
    if uid == 0 {
        return Ok(());
    }
    // SAFETY: plain syscalls; a null list with size 0 clears the groups.
    let ok = unsafe { libc::setgroups(0, std::ptr::null()) == 0 && libc::setgid(gid) == 0 && libc::setuid(uid) == 0 };
    if !ok {
        return Err(format!("cannot switch to uid {uid} (the owner of {}): {}", cfg.data_dir.display(), std::io::Error::last_os_error()));
    }
    eprintln!("railwatch: running as uid {uid} (the owner of {}), not root", cfg.data_dir.display());
    Ok(())
}

fn upgrade(rest: &[&str]) -> ExitCode {
    if unsafe { libc::geteuid() } != 0 {
        return fail("upgrade replaces /usr/local/bin/railwatch and restarts the service: run it as root (sudo railwatch upgrade)");
    }
    let base = std::env::var("RAILWATCH_RELEASE_BASE").unwrap_or_else(|_| format!("https://github.com/{}/releases", railwatch_server::REPO));
    // Tests only: a plain-http release server, whose releases are unsigned.
    let insecure = std::env::var("RAILWATCH_ALLOW_HTTP_RELEASE").is_ok_and(|v| v == "1");
    if !base.starts_with("https://") && !insecure {
        return fail(format!("{base}: releases are only fetched over https (RAILWATCH_ALLOW_HTTP_RELEASE=1 is for tests)"));
    }
    let version = flag(rest, "--version");
    let allow_downgrade = rest.contains(&"--allow-downgrade");
    // An explicit older version is refused before anything is fetched;
    // `latest` is checked once SHA256SUMS names its tag, below.
    if let Some(v) = version
        && !allow_downgrade
        && let Err(e) = not_a_downgrade(railwatch_server::VERSION, v)
    {
        return fail(e);
    }
    let dir = match version {
        Some(v) => format!("{base}/download/{v}"),
        None => format!("{base}/latest/download"),
    };
    let client = match reqwest::blocking::Client::builder().https_only(!insecure).build() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let get = |name: &str| -> Result<Vec<u8>, String> {
        let url = format!("{dir}/{name}");
        client.get(&url).send().and_then(|r| r.error_for_status()).and_then(|r| r.bytes()).map(|b| b.to_vec()).map_err(|e| format!("{url}: {e}"))
    };
    let (script, sums) = match (get("install.sh"), get("SHA256SUMS")) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return fail(e),
    };
    // The installer runs as root: it must be the one the release workflow
    // signed. SHA256SUMS carries its digest; the sigstore bundle proves
    // SHA256SUMS came from this repository's release workflow for the tag.
    let tag = match version {
        Some(v) => v.to_string(),
        // `latest`: the tag is whatever the signed SHA256SUMS names.
        None => match release_tag(&sums) {
            Some(t) => t,
            None => return fail("SHA256SUMS names no release tarball; cannot tell which release this is"),
        },
    };
    if !allow_downgrade && let Err(e) = not_a_downgrade(railwatch_server::VERSION, &tag) {
        return fail(e);
    }
    match get("SHA256SUMS.sigstore.json") {
        Ok(bundle) => {
            if let Err(e) = verify_release_signature(&sums, &bundle, &tag) {
                return fail(e);
            }
            println!("SHA256SUMS: signature verified (release workflow of {}, tag {tag})", railwatch_server::REPO);
        }
        Err(_) if insecure => {
            eprintln!("railwatch: WARNING: no signature at {dir} (RAILWATCH_ALLOW_HTTP_RELEASE test mode): install.sh is checked against SHA256SUMS only")
        }
        Err(e) => return fail(format!("{e}: the release has no signature, refusing to run its installer")),
    }
    let want =
        String::from_utf8_lossy(&sums).lines().find_map(|l| l.strip_suffix("  install.sh").or_else(|| l.strip_suffix(" *install.sh")).map(str::to_string));
    let got = railwatch_server::ingest::receipt_headers::hex(&<sha2::Sha256 as sha2::Digest>::digest(&script));
    if want.as_deref() != Some(got.as_str()) {
        return fail(format!("install.sh sha256 {got} does not match SHA256SUMS ({}); not running it", want.as_deref().unwrap_or("absent")));
    }
    println!("railwatch {} -> {tag} (from {dir})", railwatch_server::VERSION);
    // The verified bytes go to sh on stdin: nothing is written to disk
    // between the check and the run.
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-s", "--", "--upgrade", "--version", &tag]);
    // An installer from before --allow-downgrade has no downgrade check and
    // would refuse the unknown flag.
    if allow_downgrade && script.windows(19).any(|w| w == b"--allow-downgrade) ") {
        cmd.arg("--allow-downgrade");
    }
    if std::env::var_os("RAILWATCH_RELEASE_BASE").is_some() {
        // The binary and SHA256SUMS come from the same place as the script.
        cmd.args(["--release-url", &dir]);
    }
    if insecure {
        cmd.arg("--allow-http-release-url");
    }
    let mut child = match cmd.stdin(std::process::Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("sh: {e}")),
    };
    let written = child.stdin.take().expect("piped").write_all(&script);
    let status = child.wait();
    match (written, status) {
        (Ok(()), Ok(s)) if s.success() => ExitCode::SUCCESS,
        (Err(e), _) => fail(format!("sh: {e}")),
        (_, Ok(s)) => fail(format!("install.sh --upgrade exited with {s}")),
        (_, Err(e)) => fail(format!("sh: {e}")),
    }
}

/// Err when `target` is an older release than `running` (Semantic
/// Versioning 2.0 precedence, so v1.0.0-rc.1 < v1.0.0). A version that does
/// not parse, on either side, is also refused: without an order, rolling
/// back cannot be ruled out. `--allow-downgrade` skips this.
fn not_a_downgrade(running: &str, target: &str) -> Result<(), String> {
    let (Some(r), Some(t)) = (Semver::parse(running), Semver::parse(target)) else {
        return Err(format!("cannot compare versions {running:?} and {target:?}; pass --allow-downgrade if {target} is the release you want"));
    };
    if t < r {
        return Err(format!(
            "{target} is older than the running {running}; an older release may have fixed vulnerabilities. Pass --allow-downgrade to install it anyway"
        ));
    }
    Ok(())
}

/// `[v]MAJOR.MINOR.PATCH[-PRE][+BUILD]`, ordered by semver precedence (the
/// build metadata is ignored).
#[derive(Debug, PartialEq, Eq)]
struct Semver {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Semver {
    fn parse(s: &str) -> Option<Semver> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split_once('+').map_or(s, |(v, _build)| v);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut n = || -> Option<u64> {
            let p = parts.next()?;
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (p == "0" || !p.starts_with('0'))).then(|| p.parse().ok())?
        };
        let core = [n()?, n()?, n()?];
        if parts.next().is_some() {
            return None;
        }
        let pre: Vec<String> = match pre {
            None => Vec::new(),
            Some(p) => p.split('.').map(str::to_string).collect(),
        };
        if pre.iter().any(|i| i.is_empty() || !i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
            return None;
        }
        Some(Semver { core, pre })
    }
}

impl PartialOrd for Semver {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Semver {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            // A pre-release comes before its release.
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(&other.pre) {
                    let numeric = |s: &str| s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse::<u64>().ok()).flatten();
                    let o = match (numeric(a), numeric(b)) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => a.cmp(b),
                    };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                self.pre.len().cmp(&other.pre.len())
            }
        })
    }
}

/// The release tag a SHA256SUMS describes: from `railwatch-<tag>-<arch>-linux-musl.tar.gz`.
fn release_tag(sums: &[u8]) -> Option<String> {
    String::from_utf8_lossy(sums).lines().find_map(|l| {
        let name = l.split_whitespace().nth(1)?.trim_start_matches('*');
        let rest = name.strip_prefix("railwatch-")?.strip_suffix("-linux-musl.tar.gz")?;
        let (tag, _arch) = rest.rsplit_once('-')?;
        tag.starts_with('v').then(|| tag.to_string())
    })
}

/// SHA256SUMS must be signed by the release workflow for `tag`
/// (docs/SELF-HOST.md, "Releases"): checked in-process
/// (release_signature.rs), then, when cosign is installed, by cosign too.
fn verify_release_signature(sums: &[u8], bundle: &[u8], tag: &str) -> Result<(), String> {
    let valid = tag.strip_prefix('v').is_some_and(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)));
    if !valid {
        return Err(format!("{tag:?} is not a release tag"));
    }
    railwatch_server::release_signature::verify(sums, bundle, railwatch_server::REPO, tag)?;
    match cosign_verify(sums, bundle, tag) {
        Err(CosignError::NotInstalled) => Ok(()),
        Err(CosignError::Failed(e)) => Err(e),
        Ok(()) => {
            println!("SHA256SUMS: cosign agrees");
            Ok(())
        }
    }
}

enum CosignError {
    NotInstalled,
    Failed(String),
}

/// `cosign verify-blob` of SHA256SUMS against the release workflow's
/// identity for `tag`; an extra check when cosign is on PATH. `tag` is
/// already validated.
fn cosign_verify(sums: &[u8], bundle: &[u8], tag: &str) -> Result<(), CosignError> {
    let dir = private_temp_dir().map_err(|e| CosignError::Failed(format!("temporary directory: {e}")))?;
    let result = (|| {
        let write = |name: &str, bytes: &[u8]| -> Result<std::path::PathBuf, String> {
            use std::os::unix::fs::OpenOptionsExt;
            let p = dir.join(name);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&p)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            f.write_all(bytes).map_err(|e| e.to_string())?;
            Ok(p)
        };
        let (sums_path, bundle_path) =
            (write("SHA256SUMS", sums).map_err(CosignError::Failed)?, write("SHA256SUMS.sigstore.json", bundle).map_err(CosignError::Failed)?);
        let repo = railwatch_server::REPO;
        let out = std::process::Command::new("cosign")
            .arg("verify-blob")
            .arg(&sums_path)
            .arg("--bundle")
            .arg(&bundle_path)
            .args(["--certificate-identity", &format!("https://github.com/{repo}/.github/workflows/release.yml@refs/tags/{tag}")])
            .args(["--certificate-oidc-issuer", "https://token.actions.githubusercontent.com"])
            .args(["--certificate-github-workflow-trigger", "push"])
            .args(["--certificate-github-workflow-repository", repo])
            .output();
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(CosignError::Failed(format!(
                "cosign: SHA256SUMS signature does not verify for {repo} {tag}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CosignError::NotInstalled),
            Err(e) => Err(CosignError::Failed(format!("cosign: {e}"))),
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// mkdtemp(3): a new directory only this user can enter, with an
/// unpredictable name, created atomically (no symlink or squatting race).
fn private_temp_dir() -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut template = std::env::temp_dir().join("railwatch-upgrade-XXXXXX").into_os_string().into_vec();
    template.push(0);
    // SAFETY: template is a writable NUL-terminated buffer ending in XXXXXX.
    let p = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    if p.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    template.pop();
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_vec(template)))
}

// ---------------------------------------------------------------------------
// doctor

fn doctor() -> ExitCode {
    let cfg = config();
    let mut ok = true;
    println!("config file: {}", cfg.file.as_ref().map_or("(none)".to_string(), |p| p.display().to_string()));
    for (k, v, src) in cfg.describe() {
        println!("  {k:20} {v}  ({src})");
    }
    println!();
    // Data dir writable.
    match std::fs::create_dir_all(&cfg.data_dir).and_then(|_| {
        let probe = cfg.data_dir.join(".doctor-probe");
        std::fs::write(&probe, b"ok")?;
        std::fs::remove_file(&probe)
    }) {
        Ok(()) => println!("ok    data dir {} is writable", cfg.data_dir.display()),
        Err(e) => {
            ok = false;
            println!("FAIL  data dir {}: {e}", cfg.data_dir.display());
        }
    }
    // Disk space.
    if let Ok(path) = std::ffi::CString::new(cfg.data_dir.display().to_string()) {
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: path is a valid C string, s a properly sized out-param.
        if unsafe { libc::statvfs(path.as_ptr(), &mut s) } == 0 {
            let free = s.f_bavail as u64 * s.f_frsize as u64;
            let total = s.f_blocks as u64 * s.f_frsize as u64;
            let pct = if total > 0 { free as f64 * 100.0 / total as f64 } else { 0.0 };
            let line = format!("disk: {:.1} GiB free of {:.1} GiB ({pct:.0}%)", free as f64 / 1073741824.0, total as f64 / 1073741824.0);
            if pct < 10.0 {
                ok = false;
                println!("FAIL  {line}: under 10% free");
            } else {
                println!("ok    {line}");
            }
        }
    }
    // Control DB.
    match Control::open(&cfg.control_path()) {
        Ok(c) => {
            let counts = c.with(|x| {
                x.query_row("SELECT (SELECT count(*) FROM users), (SELECT count(*) FROM accounts), (SELECT count(*) FROM environments)", [], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
                })
            });
            match counts {
                Ok((u, a, e)) => {
                    println!("ok    control DB: {u} users, {a} accounts, {e} environments{}", if u == 0 { " (run `railwatch setup`)" } else { "" })
                }
                Err(e) => {
                    ok = false;
                    println!("FAIL  control DB: {e}");
                }
            }
        }
        Err(e) => {
            ok = false;
            println!("FAIL  control DB {}: {e}", cfg.control_path().display());
        }
    }
    let (files, bytes) = web::assets::embedded_bytes();
    println!("ok    embedded frontend: {files} files, {:.1} MiB, asset version {}", bytes as f64 / 1048576.0, web::assets::manifest().version);
    // SMTP.
    match &cfg.smtp {
        None => println!("warn  SMTP not configured: password resets, invitations and verification emails are logged, not sent"),
        Some(s) => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            let result = rt.block_on(async {
                let m = web::mailer::Mailer::new(s)?;
                tokio::time::timeout(Duration::from_secs(15), m.check()).await.map_err(|_| "timed out after 15 s".to_string())?
            });
            match result {
                Ok(()) => println!("ok    SMTP {}:{} accepted the connection", s.address, s.port),
                Err(e) => {
                    ok = false;
                    println!("FAIL  SMTP {}:{}: {e}", s.address, s.port);
                }
            }
        }
    }
    ok &= doctor_google(&cfg);
    if cfg.url.starts_with("http://") && !cfg.bind.starts_with("127.") && !cfg.bind.starts_with("localhost") {
        println!("warn  url is plain http on a non-loopback bind: put TLS in front and set RAILWATCH_URL=https://...");
    }
    if let Some(w) = cfg.proxy_warning() {
        println!("warn  {w}");
    }
    ok &= doctor_network(&cfg);
    ok &= doctor_tls(&cfg);
    ok &= doctor_backups(&cfg);
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Google sign-in: configured or not, the redirect URI to register, and
/// whether the key set Google signs with can be fetched from here.
fn doctor_google(cfg: &Config) -> bool {
    let Some(g) = &cfg.google else {
        println!("ok    Google sign-in not configured (set google_client_id and google_client_secret to show the button)");
        return true;
    };
    let redirect = cfg.google_redirect_uri();
    println!("ok    Google sign-in configured for client {}", g.client_id);
    println!("      authorized redirect URI to register in the Google Cloud console: {redirect}");
    let mut ok = true;
    if !redirect.starts_with("https://") && !redirect.starts_with("http://localhost") && !redirect.starts_with("http://127.0.0.1") {
        ok &= check(false, format!("Google only accepts https redirect URIs (or localhost): set url to the https address ({redirect})"));
    }
    if g.issuer.is_some() {
        println!("warn  google_issuer is set: sign-in goes to {} instead of Google (testing only)", web::google_oauth2::endpoints(g).authorize);
    }
    let jwks = web::google_oauth2::endpoints(g).jwks;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    ok & match rt.block_on(web::google_oauth2::fetch_jwks(&jwks)) {
        Ok(keys) => check(true, format!("Google signing keys: {} RSA keys from {jwks}", keys.len())),
        Err(e) => check(false, format!("Google signing keys: {e}; sign-in will fail")),
    }
}

fn check(ok: bool, line: String) -> bool {
    println!("{}  {line}", if ok { "ok  " } else { "FAIL" });
    ok
}

/// DNS for the public host, and whether the server answers on it: the
/// app's own port (the URL's), plus port 80 with built-in TLS. From this
/// box, so a firewall in front of it is only caught when the name resolves
/// to an address that routes back through it (hairpin), which most clouds do.
fn doctor_network(cfg: &Config) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let rest = cfg.url.split_once("://").map_or(cfg.url.as_str(), |(_, r)| r);
    let hostport = rest.split('/').next().unwrap_or("");
    let https = cfg.url.starts_with("https://");
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() && !h.ends_with(']') || h.starts_with('[') => (h.trim_matches(['[', ']']), p.parse().unwrap_or(443)),
        _ => (hostport.trim_matches(['[', ']']), if https { 443 } else { 80 }),
    };
    if host.is_empty() || ["127.0.0.1", "localhost", "::1"].contains(&host) {
        println!("ok    url {} is local: no DNS or public ports to check", cfg.url);
        return true;
    }
    let addrs: Vec<std::net::SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return check(false, format!("DNS: {host} does not resolve ({e}); point an A/AAAA record at this server")),
    };
    let mut ok = check(!addrs.is_empty(), format!("DNS: {host} -> {}", addrs.iter().map(|a| a.ip().to_string()).collect::<Vec<_>>().join(", ")));
    let mut ports = vec![port];
    if cfg.tls.is_some() && cfg.tls_redirect_bind.is_some() && port != 80 {
        ports.push(80);
    }
    for p in ports {
        let reached = addrs.iter().any(|a| TcpStream::connect_timeout(&std::net::SocketAddr::new(a.ip(), p), Duration::from_secs(3)).is_ok());
        let what = if p == 80 && cfg.tls.is_some() { " (ACME TLS-ALPN-01 needs 443; 80 is the redirect)" } else { "" };
        ok &= check(
            reached,
            format!(
                "port {p} on {host} {}{what}",
                if reached { "accepts connections" } else { "does not accept connections (is the server running? firewall?)" }
            ),
        );
    }
    ok
}

/// The certificate the server serves (built-in TLS): who it names and how
/// long until it expires. ACME renews with a third of the validity left
/// (30 of Let's Encrypt's 90 days), so a certificate past that point means
/// renewal is failing: FAIL. A file certificate fails under 14 days.
fn doctor_tls(cfg: &Config) -> bool {
    let Some(tls) = &cfg.tls else {
        if cfg.url.starts_with("https://") {
            println!("ok    TLS is terminated in front of railwatch (a proxy); not checked here");
        }
        return true;
    };
    match railwatch_server::tls::current_certificate(tls, &cfg.data_dir) {
        Ok(None) => check(false, "TLS: no certificate yet (ACME issues one on first start; journalctl -u railwatch shows why not)".into()),
        Ok(Some((names, not_before, not_after))) => {
            let now = railwatch_server::time::now_micros() / 1_000_000;
            let left = not_after - now;
            let (named, healthy) = match tls {
                railwatch_server::tls::Tls::Acme { domain, .. } => {
                    // Renewal starts at a third left; allow it a tenth more to finish.
                    (names.iter().any(|n| n == domain), left * 30 > (not_after - not_before) * 9)
                }
                railwatch_server::tls::Tls::Files { .. } => (true, left >= 14 * 86_400),
            };
            let when = if left >= 2 * 86_400 { format!("{} days", left / 86_400) } else { format!("{left} s") };
            let mut line = format!("TLS certificate for {} expires in {when}", names.join(", "));
            if !named {
                line.push_str(" (does not name tls_domain)");
            }
            if !healthy {
                line.push_str(if matches!(tls, railwatch_server::tls::Tls::Acme { .. }) {
                    " (past its renewal point: renewal is failing, see journalctl -u railwatch)"
                } else {
                    " (replace it)"
                });
            }
            check(named && healthy, line)
        }
        Err(e) => check(false, format!("TLS certificate: {e}")),
    }
}

/// Backups: off is a warning; on, the bucket must answer and the last
/// successful pass must be recent (3 intervals, at least 15 minutes).
fn doctor_backups(cfg: &Config) -> bool {
    let Some(b) = &cfg.backup else {
        println!("warn  backups are off: a lost disk loses everything (set backup_s3_*; docs/SELF-HOST.md)");
        return true;
    };
    let reach = b.s3.list("control/");
    let mut ok = check(
        reach.is_ok(),
        format!(
            "backup bucket {}/{}: {}",
            b.s3.endpoint,
            b.s3.bucket,
            reach.as_ref().map_or_else(|e| e.clone(), |k| format!("reachable, {} control snapshot(s)", k.len()))
        ),
    );
    let last = std::fs::read_to_string(railwatch_server::storage::backup::last_ok_path(&cfg.data_dir)).ok().and_then(|s| s.trim().parse::<i64>().ok());
    let now = railwatch_server::time::now_micros() / 1_000_000;
    let limit = (3 * b.interval_secs as i64).max(900);
    ok &= match last {
        None => check(false, "no backup pass has succeeded yet (railwatch backup runs one now)".into()),
        Some(t) => check(now - t <= limit, format!("last successful backup {} s ago (every {} s)", now - t, b.interval_secs)),
    };
    ok
}

// ---------------------------------------------------------------------------
// serve

fn serve() -> ExitCode {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_env("RAILWATCH_LOG").unwrap_or_else(|_| "info".into())).init();
    let cfg = config();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    // Held until the process exits, after the writers have drained. On
    // standby (standby_relay) the address is served, relayed to the holder,
    // while this waits, and the listener comes back with the lock.
    let (_lock, standby_listener) = match acquire_data_dir(&cfg, &runtime) {
        Ok(l) => l,
        Err(e) => return fail(e),
    };
    runtime.block_on(async {
        let control = open_control(&cfg);
        match cfg.secret() {
            Ok(secret) => control.configure_alerting(railwatch_server::alerts::integration::Alerting {
                cipher: railwatch_server::alerts::integration::SettingsCipher::new(&secret),
                smtp: cfg.smtp.clone(),
                url: cfg.pages_url(),
                deliver: cfg.alert_delivery,
            }),
            Err(e) => {
                eprintln!("railwatch: secret_key_base: {e}");
                std::process::exit(1);
            }
        }
        let (followups_tx, followups_rx) = followups::channel();
        let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(cfg.tenant_idle_seconds), followups_tx);
        let state = Arc::new(AppState {
            control: control.clone(),
            tenants: tenants.clone(),
            ip_limit: FixedWindow::new(cfg.rate_limit_ip),
            env_limit: FixedWindow::new(cfg.rate_limit_env),
            trust_proxy: cfg.trust_proxy,
            sourcemaps_dir: cfg.data_dir.join("sourcemaps"),
            cpu: Arc::new(tokio::sync::Semaphore::new(std::thread::available_parallelism().map_or(4, |n| n.get()))),
        });
        railwatch_server::grouping::set_limits(cfg.new_issues);
        // Phase 2: rollups, grouping, drain, detectors, alerts, watchdog.
        let jobs = railwatch_server::jobs::start_with(control.clone(), cfg.tenants_dir(), cfg.tiers);
        if let Some(b) = cfg.backup.clone() {
            tracing::info!(endpoint = %b.s3.endpoint, bucket = %b.s3.bucket, every = b.interval_secs, "backups on");
            railwatch_server::storage::backup::spawn(b, cfg.data_dir.clone(), cfg.tenants_dir());
        }
        let (stop_jobs, jobs_stopped) = tokio::sync::watch::channel(false);
        let scheduler = tokio::spawn(railwatch_server::jobs::scheduler(control.clone(), jobs.background.clone(), cfg.tenants_dir(), jobs_stopped));
        let hub = web::cable::Hub::new();
        let followups_task = tokio::spawn(followups::run_with(control.clone(), followups_rx, Some(hub.clone()), Some(jobs.background.clone())));
        let web_state = match WebState::new(control, cfg.clone(), tenants.clone(), hub) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!(error = e, "cannot start the web app");
                std::process::exit(1);
            }
        };
        let users: i64 = web_state.control.with(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))).unwrap_or(0);
        if users == 0 {
            tracing::warn!("no users yet: run `railwatch setup` to create the admin");
        }
        if !cfg.alert_delivery {
            tracing::warn!("alert_delivery = off: alerts are recorded as skipped and never sent");
        }
        if let Some(w) = cfg.proxy_warning() {
            tracing::warn!("{w}");
        }

        // Sockets: inherited from systemd (railwatch.socket, so they hold
        // across restarts), else bound here.
        let (inherited, how) = match inherited_listeners() {
            Ok(l) if !l.is_empty() => (l, "inherited (LISTEN_FDS)"),
            Ok(_) => (Vec::new(), "bound"),
            Err(e) => {
                eprintln!("railwatch: {e}");
                std::process::exit(1)
            }
        };
        // Two inherited sockets (railwatch.socket with a second ListenStream
        // for port 80): the one on port 80 redirects, the other serves.
        let mut inherited = inherited;
        let redirect_at = inherited.iter().position(|l| l.local_addr().is_ok_and(|a| a.port() == 80));
        let redirect_inherited = match (inherited.len(), redirect_at) {
            (2, Some(i)) => Some(inherited.remove(i)),
            (2, None) => {
                eprintln!("railwatch: LISTEN_FDS=2 but neither socket is on port 80 (the second ListenStream is the https redirect)");
                std::process::exit(1)
            }
            _ => None,
        };
        let listener = match (inherited.pop(), standby_listener) {
            (Some(l), _) => l,
            (None, Some(l)) => l,
            (None, None) => bind_or_exit(&cfg.bind).await,
        };
        // The holder also answers on serve.sock, for standby servers in other
        // network namespaces (standby.rs).
        let socket = match cfg.standby_relay.then(|| railwatch_server::standby::socket_path(&cfg.data_dir)).flatten() {
            None => None,
            Some(path) => match railwatch_server::standby::bind(&path) {
                Ok(l) => {
                    tracing::info!(socket = %path.display(), "also listening on the data dir's serve.sock for standby servers");
                    Some(l)
                }
                Err(e) => {
                    tracing::warn!(error = %e, socket = %path.display(), "cannot listen on serve.sock: standby servers cannot relay to this one");
                    None
                }
            },
        };
        let redirect = match (redirect_inherited, &cfg.tls_redirect_bind) {
            (Some(l), _) => Some(l),
            (None, Some(addr)) if cfg.tls.is_some() && how == "bound" => Some(bind_or_exit(addr).await),
            _ => None,
        };
        let tls = match &cfg.tls {
            None => None,
            Some(t) => match railwatch_server::tls::Acceptor::start(t, &cfg.data_dir).await {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("railwatch: tls: {e}");
                    std::process::exit(1)
                }
            },
        };
        let addr = listener.local_addr().map_or_else(|_| cfg.bind.clone(), |a| a.to_string());
        let scheme = if tls.is_some() { "https" } else { "http" };
        tracing::info!(addr, scheme, socket = how, url = cfg.url, data_dir = %cfg.data_dir.display(), "railwatch listening");
        let (stop_redirect, redirect_stopped) = tokio::sync::watch::channel(false);
        let redirect_task = redirect.map(|l| {
            let host = cfg.url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string();
            tracing::info!(addr = %l.local_addr().map(|a| a.to_string()).unwrap_or_default(), "redirecting http to https");
            let mut stopped = redirect_stopped;
            tokio::spawn(async move {
                let _ = axum::serve(l, railwatch_server::tls::redirect_router(host))
                    .with_graceful_shutdown(async move {
                        let _ = stopped.wait_for(|s| *s).await;
                    })
                    .await;
            })
        });
        // Pages first; anything they do not route (/ingest, /up) goes to the
        // ingest API unchanged.
        let base_path = cfg.base_path.clone();
        if !base_path.is_empty() {
            tracing::info!(base_path, "pages served under the base path; /ingest and /up stay at the root");
        }
        if cfg.trusted_proxy_secret.is_some() {
            tracing::warn!("trusted proxy auth on: a request signed with trusted_proxy_secret_file's secret is whoever it names");
        }
        let app = web::mounted(&base_path, web::router(web_state), http::router(state.clone()));
        let app = railwatch_server::serve::source_link(railwatch_server::serve::hsts(app, cfg.hsts()));
        let listener = railwatch_server::tls::Listener::new(listener, tls);
        match socket {
            None => railwatch_server::serve::run(listener, app, cfg.http, shutdown_signal()).await,
            Some(socket) => {
                let socket_app = app.clone();
                tokio::join!(
                    railwatch_server::serve::run(listener, app, cfg.http, shutdown_signal()),
                    railwatch_server::serve::run(railwatch_server::standby::Socket(socket), socket_app, cfg.http, shutdown_signal()),
                );
            }
        }
        let _ = stop_redirect.send(true);
        if let Some(t) = redirect_task {
            t.await.ok();
        }

        // In-flight requests have finished; drain the writers (they commit
        // what was queued), then the follow-ups (final usage flush).
        tracing::info!("draining tenant writers");
        drop(state);
        tokio::task::spawn_blocking(move || tenants.shutdown()).await.ok();
        followups_task.await.ok();
        let _ = stop_jobs.send(true);
        scheduler.await.ok();
        let railwatch_server::jobs::Jobs { background, threads } = jobs;
        drop(background);
        tokio::task::spawn_blocking(move || threads.into_iter().for_each(|t| drop(t.join()))).await.ok();
        tracing::info!("stopped");
    });
    ExitCode::SUCCESS
}

async fn bind_or_exit(addr: &str) -> tokio::net::TcpListener {
    tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| {
        eprintln!("railwatch: cannot bind {addr}: {e}");
        std::process::exit(1)
    })
}

/// systemd socket activation (sd_listen_fds): with LISTEN_FDS=N (and
/// LISTEN_PID, when set, naming this process) the listening sockets are fds
/// 3..3+N, opened and held by systemd (railwatch.socket, written by deploy/install.sh; with
/// built-in TLS a second ListenStream there for port 80). A restart then
/// never refuses a connection: the kernel queues it on the socket systemd
/// keeps open until the new process accepts it. `bind` is ignored then.
/// With two, the one on port 80 redirects to https and the other serves.
/// More than two is refused rather than guessed at.
fn inherited_listeners() -> Result<Vec<tokio::net::TcpListener>, String> {
    use std::os::fd::FromRawFd;
    let Ok(n) = std::env::var("LISTEN_FDS") else { return Ok(Vec::new()) };
    if let Ok(pid) = std::env::var("LISTEN_PID")
        && pid.trim() != std::process::id().to_string()
    {
        // Meant for another process (sd_listen_fds' rule): bind as usual.
        return Ok(Vec::new());
    }
    // Not inherited further (sd_listen_fds(unset_environment=1)).
    // SAFETY: the runtime's worker threads exist, but none reads or writes
    // the environment; set_var/remove_var race only with concurrent access.
    unsafe {
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    let n: i32 = match n.trim().parse() {
        Ok(n @ 1..=2) => n,
        _ => return Err(format!("LISTEN_FDS={n}: expected one socket (the app) or two (app, then port 80 for the https redirect)")),
    };
    const SD_LISTEN_FDS_START: i32 = 3;
    (SD_LISTEN_FDS_START..SD_LISTEN_FDS_START + n)
        .map(|fd| {
            // The fd must be a listening stream socket, not something else.
            let mut ty: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: valid out-pointers sized for an int option.
            let ok = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_TYPE, (&mut ty as *mut libc::c_int).cast(), &mut len) } == 0;
            let mut acc: libc::c_int = 0;
            let mut len2 = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: as above.
            let listening =
                unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN, (&mut acc as *mut libc::c_int).cast(), &mut len2) } == 0 && acc == 1;
            if !ok || ty != libc::SOCK_STREAM || !listening {
                return Err(format!("LISTEN_FDS: fd {fd} is not a listening stream socket"));
            }
            // SAFETY: fd was handed to this process for exactly this, checked
            // above, and nothing else in the process owns it.
            let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
            // SAFETY: setting close-on-exec on our own fd.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            std_listener.set_nonblocking(true).map_err(|e| format!("inherited socket: {e}"))?;
            tokio::net::TcpListener::from_std(std_listener).map_err(|e| format!("inherited socket: {e}"))
        })
        .collect()
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tag_comes_from_the_tarball_names() {
        let sums = b"aa  railwatch-v1.2.3-x86_64-linux-musl.tar.gz\nbb  railwatch-v1.2.3-aarch64-linux-musl.tar.gz\ncc  install.sh\n";
        assert_eq!(release_tag(sums).as_deref(), Some("v1.2.3"));
        assert_eq!(release_tag(b"aa  railwatch-v0.2.0-rc.1-x86_64-linux-musl.tar.gz\n").as_deref(), Some("v0.2.0-rc.1"));
        assert_eq!(release_tag(b"cc  install.sh\n"), None);
    }

    /// Issue #5: `upgrade --version` must not roll back to an older signed
    /// release (one with since-fixed vulnerabilities) unless asked to.
    #[test]
    fn upgrade_refuses_to_go_backwards() {
        assert!(not_a_downgrade("0.2.0", "v0.2.0").is_ok(), "re-installing the same release");
        assert!(not_a_downgrade("0.2.0", "v0.2.1").is_ok());
        assert!(not_a_downgrade("0.2.0", "v1.0.0").is_ok());
        assert!(not_a_downgrade("v0.2.0-rc.1", "v0.2.0").is_ok(), "a release is newer than its pre-releases");
        assert!(not_a_downgrade("0.2.0", "v0.1.9").unwrap_err().contains("--allow-downgrade"));
        assert!(not_a_downgrade("0.10.0", "v0.9.0").is_err(), "numeric, not lexical");
        assert!(not_a_downgrade("0.2.0", "v0.2.0-rc.1").is_err(), "a pre-release of the running version is older");
        assert!(not_a_downgrade("0.0.0-test.10", "v0.0.0-test.9").is_err());
        assert!(not_a_downgrade("0.0.0-test.2", "v0.0.0-test.10").is_ok(), "numeric pre-release identifiers compare as numbers");
        assert!(not_a_downgrade("1.0.0-alpha.1", "v1.0.0-alpha").is_err(), "fewer identifiers sort first");
        assert!(not_a_downgrade("1.0.0-alpha", "v1.0.0-beta").is_ok());
        assert!(not_a_downgrade("1.0.0-rc.1", "v1.0.0-1").is_err(), "numeric identifiers sort before alphanumeric ones");
        assert!(not_a_downgrade("1.0.0+build.5", "v1.0.0+build.1").is_ok(), "build metadata is ignored");
        assert!(not_a_downgrade("0.2.0", "latest").is_err(), "an unparseable version is refused");
        assert!(not_a_downgrade("0.2.0", "v01.2.3").is_err(), "leading zeros are not semver");
        assert!(not_a_downgrade("0.2.0", "v1.2").is_err());
    }

    /// The upgrade's scratch directory is fresh, private and unpredictable.
    #[test]
    fn upgrade_temp_dir_is_private_and_unique() {
        use std::os::unix::fs::PermissionsExt;
        let (a, b) = (private_temp_dir().unwrap(), private_temp_dir().unwrap());
        assert_ne!(a, b);
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(!a.to_string_lossy().ends_with("XXXXXX"));
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
    }

    /// The signature check fails closed and needs no cosign (issue #7):
    /// the real v0.1.0 release (tests/fixtures/release-v0.1.0) verifies for
    /// its own tag; another tag, one changed byte, a bogus bundle or a tag
    /// that is not one does not. With cosign on PATH it must agree too.
    #[test]
    fn release_signature_fails_closed() {
        assert!(verify_release_signature(b"x", b"{}", "not-a-tag").unwrap_err().contains("not a release tag"));
        assert!(verify_release_signature(b"x", b"{}", "v1.0.0; rm -rf /").is_err());
        assert!(verify_release_signature(b"x", b"{}", "v1.0.0").is_err(), "a bogus bundle never verifies");
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/release-v0.1.0");
        let sums = std::fs::read(format!("{dir}/SHA256SUMS")).unwrap();
        let bundle = std::fs::read(format!("{dir}/SHA256SUMS.sigstore.json")).unwrap();
        assert_eq!(release_tag(&sums).as_deref(), Some("v0.1.0"));
        verify_release_signature(&sums, &bundle, "v0.1.0").expect("the v0.1.0 release signature verifies");
        assert!(verify_release_signature(&sums, &bundle, "v0.1.1").is_err(), "the signature is pinned to its tag");
        let mut tampered = sums.clone();
        tampered[0] ^= 1;
        assert!(verify_release_signature(&tampered, &bundle, "v0.1.0").is_err(), "a changed SHA256SUMS does not verify");
        match cosign_verify(&sums, &bundle, "v0.1.0") {
            Err(CosignError::NotInstalled) => eprintln!("cosign is not on PATH: verified in-process only"),
            Err(CosignError::Failed(e)) => panic!("cosign disagrees: {e}"),
            Ok(()) => assert!(matches!(cosign_verify(&tampered, &bundle, "v0.1.0"), Err(CosignError::Failed(_)))),
        }
    }
}
