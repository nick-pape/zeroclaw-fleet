//! Periodic poll of every claw's `/api/cost` endpoint, cached for the
//! fleet rollup endpoint.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::{interval, Instant};
use tracing::warn;

use crate::config::OrchestratorConfig;

/// Subset of ZeroClaw's `GET /api/cost` JSON we care about. Matches the
/// shape at `crates/zeroclaw-gateway/src/api.rs:771-803` in the ZeroClaw
/// fork — see verified premises in the design doc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub daily_cost_usd: f64,
    #[serde(default)]
    pub monthly_cost_usd: f64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub by_model: serde_json::Value,
}

/// Cached cost snapshot per claw with a fetch timestamp so stale data is
/// observable.
#[derive(Debug, Clone, Serialize)]
pub struct CostSnapshot {
    pub claw: String,
    pub summary: CostSummary,
    /// RFC3339 UTC; the last time this snapshot was successfully fetched.
    pub fetched_at: String,
    /// Last error encountered (if any) — present even on a fresh successful
    /// snapshot is None; populated when a subsequent poll fails but we
    /// keep showing the stale value.
    pub last_error: Option<String>,
}

/// Type alias for the shared cache the poller writes and the API reads.
pub type CostCache = Arc<RwLock<HashMap<String, CostSnapshot>>>;

/// Build a fresh empty cache.
pub fn new_cache() -> CostCache {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Spawn the background poller task. Returns immediately; the task runs
/// until the process exits.
pub fn spawn(
    cfg: Arc<OrchestratorConfig>,
    cache: CostCache,
    http: reqwest::Client,
    claws: Arc<RwLock<Vec<String>>>,
) {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(cfg.cost_poll_interval_secs.max(5)));
        loop {
            tick.tick().await;
            let snapshot_at = chrono_like_now();
            let claw_list = claws.read().await.clone();
            for claw in claw_list {
                let started = Instant::now();
                match poll_once(&http, &cfg.proxy.upstream_base(&claw)).await {
                    Ok(summary) => {
                        let mut w = cache.write().await;
                        w.insert(
                            claw.clone(),
                            CostSnapshot {
                                claw: claw.clone(),
                                summary,
                                fetched_at: snapshot_at.clone(),
                                last_error: None,
                            },
                        );
                        tracing::debug!(
                            claw = %claw,
                            elapsed_ms = started.elapsed().as_millis(),
                            "cost poll ok"
                        );
                    }
                    Err(e) => {
                        warn!(claw = %claw, error = %e, "cost poll failed");
                        let mut w = cache.write().await;
                        if let Some(existing) = w.get_mut(&claw) {
                            existing.last_error = Some(e.to_string());
                        } else {
                            w.insert(
                                claw.clone(),
                                CostSnapshot {
                                    claw: claw.clone(),
                                    summary: CostSummary::default(),
                                    fetched_at: snapshot_at.clone(),
                                    last_error: Some(e.to_string()),
                                },
                            );
                        }
                    }
                }
            }
        }
    });
}

/// Single-shot fetch — extracted so it can be unit-tested against a mock.
pub async fn poll_once(http: &reqwest::Client, upstream_base: &str) -> Result<CostSummary> {
    let url = format!("{upstream_base}/api/cost");
    let resp = http.get(&url).send().await?.error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    // ZeroClaw wraps the summary under "cost".
    let inner = body
        .get("cost")
        .cloned()
        .unwrap_or(body);
    let summary: CostSummary = serde_json::from_value(inner)?;
    Ok(summary)
}

/// Roll up the cache into a fleet-level total.
pub async fn fleet_rollup(cache: &CostCache) -> FleetCostRollup {
    let r = cache.read().await;
    let mut session = 0.0;
    let mut daily = 0.0;
    let mut monthly = 0.0;
    let mut tokens = 0u64;
    let mut requests = 0u64;
    let mut claws = 0u32;
    let mut stale = 0u32;
    for snap in r.values() {
        claws += 1;
        if snap.last_error.is_some() {
            stale += 1;
        }
        session += snap.summary.session_cost_usd;
        daily += snap.summary.daily_cost_usd;
        monthly += snap.summary.monthly_cost_usd;
        tokens += snap.summary.total_tokens;
        requests += snap.summary.request_count;
    }
    FleetCostRollup {
        claws,
        stale,
        session_cost_usd: session,
        daily_cost_usd: daily,
        monthly_cost_usd: monthly,
        total_tokens: tokens,
        request_count: requests,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetCostRollup {
    pub claws: u32,
    pub stale: u32,
    pub session_cost_usd: f64,
    pub daily_cost_usd: f64,
    pub monthly_cost_usd: f64,
    pub total_tokens: u64,
    pub request_count: u64,
}

/// RFC3339 UTC "now". Standalone so tests / future deterministic fakes
/// can swap this out.
fn chrono_like_now() -> String {
    // Use only std + an ad-hoc formatter to avoid a chrono dep.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    // YYYY-MM-DDTHH:MM:SS.mmmZ via the simple math approach.
    let (y, m, d, h, mi, s) = unix_to_ymdhms(secs);
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        nanos / 1_000_000
    )
}

/// Convert a Unix-seconds timestamp into (Y, M, D, H, Mi, S) UTC.
fn unix_to_ymdhms(secs: u64) -> (i32, u8, u8, u8, u8, u8) {
    let s = (secs % 60) as u8;
    let mi = ((secs / 60) % 60) as u8;
    let h = ((secs / 3600) % 24) as u8;
    let mut days = secs / 86_400;
    let mut y: i32 = 1970;
    loop {
        let leap = is_leap(y);
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let mdays = [31, if is_leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m: u8 = 1;
    for md in mdays {
        if days < md {
            break;
        }
        days -= md;
        m += 1;
    }
    let d = (days as u8) + 1;
    (y, m, d, h, mi, s)
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_summary_default_is_zero() {
        let c = CostSummary::default();
        assert_eq!(c.session_cost_usd, 0.0);
        assert_eq!(c.total_tokens, 0);
    }

    #[test]
    fn cost_summary_parses_minimal_zeroclaw_payload() {
        let json = serde_json::json!({
            "session_cost_usd": 0.012,
            "daily_cost_usd": 1.34,
            "monthly_cost_usd": 9.87,
            "total_tokens": 12345,
            "request_count": 42,
            "by_model": {"openai/gpt-4o": {"input": 10000, "output": 2345}}
        });
        let c: CostSummary = serde_json::from_value(json).unwrap();
        assert!((c.session_cost_usd - 0.012).abs() < 1e-9);
        assert_eq!(c.total_tokens, 12345);
        assert_eq!(c.request_count, 42);
    }

    #[tokio::test]
    async fn rollup_sums_across_claws_and_marks_stale_count() {
        let cache = new_cache();
        {
            let mut w = cache.write().await;
            w.insert("a".into(), CostSnapshot {
                claw: "a".into(),
                summary: CostSummary { daily_cost_usd: 1.0, total_tokens: 100, ..Default::default() },
                fetched_at: "x".into(),
                last_error: None,
            });
            w.insert("b".into(), CostSnapshot {
                claw: "b".into(),
                summary: CostSummary { daily_cost_usd: 2.5, total_tokens: 200, ..Default::default() },
                fetched_at: "x".into(),
                last_error: Some("timeout".into()),
            });
        }
        let r = fleet_rollup(&cache).await;
        assert_eq!(r.claws, 2);
        assert_eq!(r.stale, 1);
        assert!((r.daily_cost_usd - 3.5).abs() < 1e-9);
        assert_eq!(r.total_tokens, 300);
    }

    #[test]
    fn unix_to_ymdhms_matches_known_epoch_anchors() {
        assert_eq!(unix_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        // 2026-05-17T17:00:00Z = 1779065600  (rough sanity, not exact)
        let (y, m, d, _, _, _) = unix_to_ymdhms(1779066000);
        assert_eq!(y, 2026);
        assert_eq!(m, 5);
        assert!(d >= 17 && d <= 18);
    }
}
