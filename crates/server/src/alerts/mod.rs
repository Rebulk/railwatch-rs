//! Alerts: rule matching and firing (AlertRule), the outbox with leases and
//! retries (Alert, DeliverAlertJob, ReconcileAlertDeliveriesJob,
//! SummarizeAlertBurstJob), delivery (Alerts::Delivery) and the outbound
//! address policy (Alerts::OutboundHttp).

pub mod delivery;
pub mod integration;
pub mod outbound_http;
pub mod outbox;
pub mod rules;

use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use crate::control::Control;

/// Shared outbox health for the dead-man switch.
#[derive(Default)]
pub struct OutboxHealth {
    /// epoch seconds of the last delivery pass that ran to completion
    pub last_pass: AtomicI64,
}

/// One reconcile + delivery pass: recover stale leases, summarize closed
/// bursts, claim due alerts, deliver them concurrently, record outcomes.
pub async fn deliver_pass(control: &Arc<Control>) -> Result<usize, String> {
    let c = control.clone();
    let claimed = tokio::task::spawn_blocking(move || {
        let conn = c.conn();
        outbox::summarize_bursts(&conn)?;
        let claimed = outbox::claim_due(&conn, outbox::BATCH_SIZE)?;
        claimed.into_iter().map(|a| delivery::Context::load(&conn, c.alerting(), a)).collect::<rusqlite::Result<Vec<_>>>()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let n = claimed.len();
    let smtp = control.alerting().and_then(|a| a.smtp.clone());
    let deliver = delivering(control);
    let results = futures_join(claimed.into_iter().map(|ctx| {
        let smtp = smtp.clone();
        async move {
            let r = if ctx.enabled && deliver { delivery::deliver(&ctx, smtp.as_ref()).await.map(|_| "sent") } else { Ok("skipped") };
            (ctx, r)
        }
    }))
    .await;
    let c = control.clone();
    tokio::task::spawn_blocking(move || {
        let conn = c.conn();
        for (ctx, r) in results {
            match r {
                Ok(outcome) => {
                    outbox::finish(&conn, &ctx.alert, outcome)?;
                }
                Err(e) => {
                    tracing::warn!(alert = ctx.alert.id, error = %e, "alert delivery failed");
                    outbox::fail(&conn, &ctx.alert, &delivery::error_with_class(&e))?;
                }
            }
        }
        Ok::<_, rusqlite::Error>(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(n)
}

/// Integration#deliver_test!: one synthetic alert (event new_issue,
/// payload test/environment/message), delivered now rather than through the
/// loop, so the operator sees the real outcome. The row is created already
/// claimed, so the delivery loop never picks it up as well. Ok(outcome) is
/// "sent" or "skipped" (disabled); Err is the failure, also stored on the
/// alert (failed) and the integration (last_error), as Alert#deliver!.
pub async fn deliver_test(control: &Arc<Control>, integration_id: i64) -> Result<&'static str, String> {
    let c = control.clone();
    let ctx = tokio::task::spawn_blocking(move || {
        let conn = c.conn();
        let (name, kind): (String, String) =
            conn.query_row("SELECT name, kind FROM integrations WHERE id = ?", [integration_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let payload = crate::rhash! {
            "test" => true, "environment" => "production",
            "message" => format!("Test alert from Railwatch: {name} ({kind}) is wired up correctly."),
        };
        let claimed = outbox::create_claimed(&conn, integration_id, "new_issue", &payload)?;
        delivery::Context::load(&conn, c.alerting(), claimed)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let smtp = control.alerting().and_then(|a| a.smtp.clone());
    let result = if ctx.enabled && delivering(control) { delivery::deliver(&ctx, smtp.as_ref()).await.map(|_| "sent") } else { Ok("skipped") };
    let c = control.clone();
    let r2 = result.clone();
    tokio::task::spawn_blocking(move || {
        let conn = c.conn();
        match r2 {
            Ok(outcome) => outbox::finish(&conn, &ctx.alert, outcome).map(drop),
            Err(e) => outbox::fail_now(&conn, &ctx.alert, &delivery::error_with_class(&e)),
        }
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    result
}

/// False when the server runs with `alert_delivery = "off"` (a shadow).
fn delivering(control: &Control) -> bool {
    control.alerting().is_none_or(|a| a.deliver)
}

async fn futures_join<F: std::future::Future + Send + 'static>(futs: impl Iterator<Item = F>) -> Vec<F::Output>
where
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

/// The Hash inside a Value built with rhash!.
pub fn as_hash(v: crate::rjson::Value) -> crate::rjson::Hash {
    match v {
        crate::rjson::Value::Hash(h) => h,
        _ => crate::rjson::Hash::new(),
    }
}
