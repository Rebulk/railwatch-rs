//! What a batch still owes once its rows have committed (Ingest::Followups,
//! Batch#enqueue_followups): counting its events against the quota, grouping
//! its exceptions into issues, folding it into the hourly rollups, and
//! telling the watchdog the environment (and each server) is alive.
//!
//! - usage is aggregated in memory and flushed to the control database once
//!   per second, one UPDATE per environment and month (docs/DIFFERENCES.md);
//! - heartbeats: last-seen per (environment, server) in memory, from the
//!   committed rows themselves, no database read per batch (jobs::watchdog);
//! - direct-path exceptions are grouped right after commit (GroupExceptionsJob);
//!   receipted batches' follow-ups (grouping under claims, usage under claims
//!   in Rails) are drained from ingest_receipts by jobs::drain, which survives
//!   a crash between commit and here;
//! - rollups: each batch's new row ids go to the tenant's incremental
//!   accumulator (rollup::incremental) and dirty groups are flushed every
//!   ROLLUP_FLUSH; the full RollupJob recompute stays as catch-up/repair
//!   (jobs::rollup_catchup).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::control::{Control, month_of};
use crate::ingest::batch::Prepared;
use crate::ingest::mapper::Bind;
use crate::ingest::writer::Written;

pub const ROLLUP_FLUSH: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Followup {
    pub env_id: i64,
    pub slug: String,
    pub accepted: i64,
    pub committed_at_micros: i64,
    pub rollup_buckets: Vec<i64>,
    pub session_buckets: Vec<i64>,
    pub exception_ids: Vec<i64>,
    /// Records per type (`@counts`), for the live-update broadcast.
    pub counts: Vec<(String, i64)>,
    /// Receipted batches leave grouping to the drain (claims per group).
    pub receipted: bool,
    pub rolled_ids: Vec<(&'static str, Vec<i64>)>,
    /// Servers named on this batch's rows ("" when a row had none).
    pub servers: Vec<String>,
}

impl Followup {
    pub fn from_batch(env_id: i64, slug: &str, batch: &Prepared, written: &Written) -> Followup {
        let mut servers: Vec<String> = Vec::new();
        for (_, rows) in &batch.tables {
            for row in rows {
                let s = match row.iter().find(|(c, _)| *c == "server") {
                    Some((_, Bind::Text(s))) => s.as_str(),
                    _ => "",
                };
                if !servers.iter().any(|x| x == s) {
                    servers.push(s.to_string());
                }
            }
        }
        Followup {
            env_id,
            slug: slug.to_string(),
            accepted: written.accepted,
            committed_at_micros: written.committed_at_micros.unwrap_or(batch.meta.received_at_micros),
            rollup_buckets: batch.rollup_buckets.iter().copied().collect(),
            session_buckets: batch.session_buckets.iter().copied().collect(),
            exception_ids: written.exception_ids.clone(),
            counts: batch.counts.iter().map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), *v)).collect(),
            receipted: batch.receipt.is_some(),
            rolled_ids: written.rolled_ids.clone(),
            servers,
        }
    }
}

#[derive(Clone)]
pub struct FollowupSender(mpsc::UnboundedSender<Followup>);

impl FollowupSender {
    pub fn send(&self, f: Followup) {
        let _ = self.0.send(f);
    }
}

pub fn channel() -> (FollowupSender, mpsc::UnboundedReceiver<Followup>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (FollowupSender(tx), rx)
}

/// Where the follow-ups hand work on. Without a Background (tests, CLI)
/// only usage is counted, as in Phase 1.
#[derive(Clone)]
pub struct Background {
    pub heartbeats: Arc<crate::jobs::watchdog::Heartbeats>,
    pub work: std::sync::mpsc::Sender<crate::jobs::TenantWork>,
}

#[derive(Default)]
struct Pending {
    /// (env, month) -> (events, latest commit time)
    usage: HashMap<(i64, String), (i64, i64)>,
}

/// Runs until the channel closes (every sender dropped), then flushes once
/// more, so a graceful shutdown loses no usage.
pub async fn run(control: Arc<Control>, rx: mpsc::UnboundedReceiver<Followup>) {
    run_with(control, rx, None, None).await
}

/// `run`, also broadcasting each committed batch to the environment's live
/// subscribers (Ingest::Batch#broadcast_live, throttled in the hub) and
/// handing it to the background work (rollups, grouping, watchdog).
pub async fn run_with(control: Arc<Control>, mut rx: mpsc::UnboundedReceiver<Followup>, cable: Option<crate::web::cable::Hub>, bg: Option<Background>) {
    let mut pending = Pending::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(f) => {
                    if let Some(hub) = &cable {
                        hub.ingested(f.env_id, &f.counts);
                    }
                    handle(&mut pending, f, bg.as_ref())
                }
                None => break,
            },
            _ = tick.tick() => flush(&control, &mut pending).await,
        }
    }
    flush(&control, &mut pending).await;
}

fn handle(p: &mut Pending, f: Followup, bg: Option<&Background>) {
    if f.accepted > 0 {
        let e = p.usage.entry((f.env_id, month_of(f.committed_at_micros))).or_insert((0, 0));
        e.0 += f.accepted;
        e.1 = e.1.max(f.committed_at_micros);
    }
    let Some(bg) = bg else { return };
    bg.heartbeats.beat(f.env_id, &f.servers, f.committed_at_micros);
    let _ = bg.work.send(crate::jobs::TenantWork::Committed(f));
}

async fn flush(control: &Arc<Control>, p: &mut Pending) {
    if p.usage.is_empty() {
        return;
    }
    let usage = std::mem::take(&mut p.usage);
    let control = control.clone();
    let result = tokio::task::spawn_blocking(move || {
        for ((env, month), (n, at)) in &usage {
            control.count_events(*env, month, *n, &crate::time::quoted_date_micros(*at))?;
        }
        Ok::<_, rusqlite::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "usage flush failed; those events are not counted"),
        Err(e) => tracing::error!(error = %e, "usage flush task failed"),
    }
}
