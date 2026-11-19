//! GA tenancy review, 2026-11-19, T1/T5/P2: a deleted environment's slug
//! (its telemetry directory, source maps and backup objects) is never given
//! to another environment, and deleting one closes every reader and writer
//! of its files first. The reviewer's reproduction (a reader pooled before
//! the delete served the deleted company's rows to whoever recreated the
//! slug) is the first test, inverted.

use std::path::{Path, PathBuf};
use std::time::Duration;

use railwatch_server::control::{self, Control};
use railwatch_server::dashboard::pool::ReadPools;
use railwatch_server::storage::layout::{self, Tier};

fn root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rw-tenancy-ga-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_log(dir: &Path, day: &str, message: &str) {
    layout::ensure(dir).unwrap();
    let c = layout::open_or_create(&layout::day_path(dir, Tier::Detail, day)).unwrap();
    c.execute("INSERT INTO logs (level, message, occurred_at, tags) VALUES ('info', ?1, '2026-11-19 00:00:00', '[]')", [message]).unwrap();
    drop(layout::open_or_create(&layout::day_path(dir, Tier::Core, day)).unwrap());
}

fn messages(pools: &ReadPools, slug: &str) -> Vec<String> {
    let r = pools.get(slug).unwrap();
    let mut s = r.prepare("SELECT message FROM logs ORDER BY message").unwrap();
    s.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

fn open_fds(root: &Path) -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().flatten().filter_map(|e| std::fs::read_link(e.path()).ok()).filter(|t| t.starts_with(root)).count()
}

/// Deleting an environment forgets its pool before the files go: no handle
/// to them is left, and nothing is read under that name again.
#[test]
fn deleting_a_tenant_closes_its_pooled_readers_so_a_new_directory_is_never_read_through_them() {
    let tenants = root("t1");
    let slug = "omega-shop-production";
    let dir = tenants.join(slug);
    let pools = ReadPools::with_max(tenants.clone(), 4, 16);
    write_log(&dir, "2026-11-19", "A-PRIVATE customer=jane@example.com");
    assert_eq!(messages(&pools, slug), ["A-PRIVATE customer=jane@example.com"]);
    // A reader checked out while the delete runs closes when it comes back.
    let busy = pools.get(slug).unwrap();

    // destroy_tenant: forget, then remove.
    pools.forget(slug);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(busy);
    assert_eq!(open_fds(&tenants), 0, "no reader keeps the deleted files open");
    assert_eq!(pools.open_count(), 0);

    // Even if something recreated the directory, the forgotten slug is not
    // read again (slugs are never reused; PR #107 review L9).
    write_log(&dir, "2026-11-19", "B-OWN");
    assert!(pools.get(slug).is_err(), "never the deleted tenant's rows, nor anything under its name");
    let _ = std::fs::remove_dir_all(&tenants);
}

fn account_app(c: &Control, account: &str, app: &str) -> i64 {
    let email = format!("{}@example.com", control::base58(10).to_lowercase());
    let user = c.tx(|tx| control::insert_user(tx, account, &email, "correct horse battery", true)).unwrap();
    let acct = c.tx(|tx| control::create_account(tx, account, user)).unwrap();
    c.tx(|tx| control::insert_application(tx, acct, app, &control::parameterize(app), "SHOP", None, "main")).unwrap()
}

/// The slug itself is never reused: an environment, application or account
/// deleted and recreated with the same names gets a new slug, so nothing
/// keyed by the slug (telemetry, source maps, backup objects, writers,
/// worker state) can carry over (T1, T5, P2).
#[test]
fn a_deleted_environments_slug_is_never_given_to_a_new_environment() {
    let d = root("slug");
    let c = Control::open(&d.join("control.sqlite3")).unwrap();
    let app = account_app(&c, "Omega", "Shop");
    let (env, _) = c.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    let slug = |id: i64| c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [id], |r| r.get::<_, String>(0))).unwrap();
    assert_eq!(slug(env), "omega-shop-production");
    c.with(|x| x.execute("DELETE FROM environments WHERE id = ?", [env])).unwrap();
    let (again, _) = c.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    assert_eq!(slug(again), "omega-shop-production-2");
    // Account and application gone too (their slugs are free again), then
    // someone else creates the same names: still a new environment slug.
    c.with(|x| x.execute_batch("DELETE FROM environments; DELETE FROM applications; DELETE FROM memberships; DELETE FROM accounts;")).unwrap();
    let app = account_app(&c, "Omega", "Shop");
    let (third, _) = c.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    assert_eq!(slug(third), "omega-shop-production-3");
    // `railwatch env create <slug>` with a retired slug is refused.
    assert!(c.create("omega-shop-production", &"0".repeat(64), 30).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

/// Telemetry left on disk by an environment deleted before this fix (no
/// retired row yet) retires its slug at startup, as do backup objects.
#[test]
fn unowned_telemetry_and_backup_objects_retire_their_slugs() {
    let d = root("unowned");
    let c = Control::open(&d.join("control.sqlite3")).unwrap();
    let app = account_app(&c, "Acme", "Web");
    let (env, _) = c.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    let tenants = d.join("telemetry");
    layout::ensure(&tenants.join("acme-web-production")).unwrap();
    layout::ensure(&tenants.join("acme-web-staging")).unwrap();
    std::fs::write(tenants.join("acme-web-old.sqlite3"), b"").unwrap();
    std::fs::create_dir_all(d.join("sourcemaps/acme-web-qa/d1")).unwrap();
    let names = control::storage_names(&tenants, &d.join("sourcemaps"));
    assert_eq!(c.with(|x| control::retire_unowned(x, names.clone())).unwrap(), 3, "staging, old, qa; not the live one");
    assert_eq!(c.with(|x| control::retire_unowned(x, names)).unwrap(), 0, "idempotent");
    let keys = ["tenants/acme-web-ci/meta.sqlite3.zst".to_string(), "tenants/acme-web-rails.sqlite3.zst".to_string()];
    let bucket = railwatch_server::storage::backup::bucket_slugs(keys.iter());
    assert_eq!(bucket, ["acme-web-ci", "acme-web-rails"]);
    assert_eq!(c.with(|x| control::retire_unowned(x, bucket)).unwrap(), 2);
    for name in ["staging", "old", "qa", "ci", "rails"] {
        let (id, _) = c.tx(|tx| control::create_environment(tx, app, name)).unwrap();
        let s: String = c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [id], |r| r.get(0))).unwrap();
        assert_eq!(s, format!("acme-web-{name}-2"));
    }
    assert!(c.with(|x| control::slug_retired(x, "acme-web-production")).map(|r| !r).unwrap());
    let _ = env;
    let _ = std::fs::remove_dir_all(&d);
}

/// A writer for a deleted environment is stopped (its connections closed)
/// before the files are removed, and a batch that authenticated before the
/// delete is refused instead of recreating the directory (T5).
#[test]
fn deleting_a_tenant_stops_its_writer_and_refuses_later_batches() {
    use railwatch_server::tenants::{Job, Tenants};
    let d = root("writer");
    let (tx, _rx) = railwatch_server::followups::channel();
    let tenants = Tenants::new(d.join("telemetry"), Duration::from_secs(300), tx);
    let slug = "acme-web-production";
    tenants.ensure(slug).unwrap();
    let lookup = |t: &Tenants| {
        let (reply, rx) = tokio::sync::oneshot::channel();
        t.send(1, slug, Job::LookupReceipt { producer_id: "p".into(), delivery_id: "d".into(), reply });
        rx.blocking_recv().unwrap()
    };
    assert!(lookup(&tenants).unwrap().is_none(), "a running writer");
    assert!(open_fds(&d) > 0);
    tenants.forget(slug, Duration::from_secs(5));
    assert_eq!(open_fds(&d), 0, "the writer closed its files before returning");
    std::fs::remove_dir_all(d.join("telemetry").join(slug)).unwrap();
    assert!(lookup(&tenants).is_err(), "refused, not a new writer");
    assert!(!d.join("telemetry").join(slug).exists(), "nothing recreated the directory");
    tenants.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}
