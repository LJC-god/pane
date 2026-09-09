//! GLM Coding Plan — China region (智谱 open.bigmodel.cn).
//!
//! The China twin of the Z.ai provider: same wire shape at a different
//! host. GET https://open.bigmodel.cn/api/monitor/usage/quota/limit with
//! the Coding Plan key returns `{ data: { level, limits: [...] } }` where
//! `limits` carries two TOKENS_LIMIT windows (5-hour Session and Weekly,
//! told apart by their nextResetTime — the earlier reset is the session)
//! plus a monthly TIME_LIMIT for MCP-tool calls. Only `percentage` (used)
//! is reported for token windows.
//!
//! This provider has no implicit source — credentials exist only as named
//! accounts (Settings → Accounts), one card per key, like omp's explicit
//! credential list.

use super::{http, Metric, Snapshot};
use serde_json::Value;
use std::time::Duration;

const NAME_PREFIX: &str = "GLM CN";
const QUOTA_URL: &str = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const MAX_QUOTA_BYTES: usize = 256 * 1024;

/// One named account → one card. `id` is the account id; the snapshot id
/// is `glm_cn@<id>`, the card name is the user's label.
pub async fn snapshot_named(id: String, name: String, key: String) -> Snapshot {
    let snap_id = format!("glm_cn@{id}");
    let card_name = format!("{NAME_PREFIX} — {name}");
    match fetch(&key).await {
        Ok(s) => Snapshot {
            id: snap_id,
            name: card_name,
            ..s
        },
        Err(e) => Snapshot::error(&snap_id, &card_name, e),
    }
}

async fn fetch(key: &str) -> Result<Snapshot, String> {
    // Community tooling sends the key raw (no Bearer prefix) on this
    // endpoint; Bearer is what the rest of bigmodel's API expects. Try
    // Bearer first — it is the documented convention — and fall back to
    // the raw token on rejection so either spelling works.
    let doc = match quota_doc(key, true).await {
        Ok(doc) => doc,
        Err(QuotaError::Rejected) => quota_doc(key, false).await.map_err(|e| match e {
            QuotaError::Rejected => {
                "Coding Plan key was rejected — check it in Settings → Accounts".into()
            }
            QuotaError::Other(e) => e,
        })?,
        Err(QuotaError::Other(e)) => return Err(e),
    };
    let data = doc.get("data").unwrap_or(&doc);
    let plan = data
        .get("level")
        .and_then(Value::as_str)
        .filter(|l| !l.trim().is_empty())
        .map(title_case_first)
        .or_else(|| {
            data.get("planName")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let metrics = parse_limits(
        data.get("limits").and_then(Value::as_array),
    )?;
    Ok(Snapshot::ok("glm_cn", NAME_PREFIX, plan, metrics))
}

enum QuotaError {
    Rejected,
    Other(String),
}

/// "pro" → "Pro" (char-based so an empty or non-ASCII level never panics).
fn title_case_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

async fn quota_doc(key: &str, bearer: bool) -> Result<Value, QuotaError> {
    let mut req = http()
        .get(QUOTA_URL)
        .timeout(Duration::from_secs(10))
        .header("Accept", "application/json");
    req = if bearer {
        req.bearer_auth(key)
    } else {
        req.header("Authorization", key)
    };
    let resp = req
        .send()
        .await
        .map_err(|e| QuotaError::Other(format!("usage request: {e}")))?;
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(QuotaError::Rejected);
    }
    if !status.is_success() {
        return Err(QuotaError::Other(format!("usage endpoint: HTTP {status}")));
    }
    super::json_body(resp, MAX_QUOTA_BYTES, "usage").await.map_err(QuotaError::Other)
}

/// limits → meters. Token windows (TOKENS_LIMIT) come as bare used
/// percentages with a reset timestamp; sorted by nextResetTime the first
/// is the rolling 5-hour session and the second the weekly window (this is
/// how the vendor's own usage plugin tells them apart). TIME_LIMIT is the
/// monthly MCP-tool quota with inverted field roles (`usage` = cap,
/// `currentValue` = used) — same quirk as the Z.ai endpoint.
fn parse_limits(limits: Option<&Vec<Value>>) -> Result<Vec<Metric>, String> {
    let Some(limits) = limits else {
        return Err("usage response had no limits array".into());
    };
    let mut token_limits: Vec<&Value> = Vec::new();
    let mut monthly: Option<&Value> = None;
    for entry in limits {
        match entry.get("type").and_then(Value::as_str).unwrap_or("") {
            "TOKENS_LIMIT" => token_limits.push(entry),
            "TIME_LIMIT" => monthly = Some(entry),
            _ => {}
        }
    }
    // Session and Weekly are the headline meters; the monthly MCP-tool
    // quota reads last so the card opens on the windows users pace against.
    let mut metrics = Vec::new();
    token_limits.sort_by_key(|e| {
        e.get("nextResetTime")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });
    for (idx, entry) in token_limits.iter().enumerate() {
        let (label, period_ms) = if idx == 0 {
            ("Session", Some(5 * HOUR_MS))
        } else {
            ("Weekly", Some(7 * DAY_MS))
        };
        // A missing percentage on a token window is a shape change, not a
        // zero — fail the card rather than render a silent 0%.
        let Some(used) = entry.get("percentage").and_then(Value::as_f64) else {
            return Err("token limit entry had no percentage".into());
        };
        let resets_at = entry
            .get("nextResetTime")
            .and_then(Value::as_i64)
            .filter(|ms| *ms > 0);
        metrics.push(
            Metric::progress(label, used.clamp(0.0, 100.0), None).with_reset(resets_at, period_ms),
        );
    }
    if let Some(entry) = monthly {
        let used = entry.get("currentValue").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let cap = entry.get("usage").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        if cap > 0.0 {
            let resets_at = entry
                .get("nextResetTime")
                .and_then(Value::as_i64)
                .filter(|ms| *ms > 0);
            metrics.push(
                Metric::progress(
                    "MCP calls",
                    (used / cap * 100.0).clamp(0.0, 100.0),
                    Some(format!("{used:.0} of {cap:.0} calls")),
                )
                .with_reset(resets_at, Some(30 * DAY_MS)),
            );
        }
    }
    if metrics.is_empty() {
        return Err("usage response had no recognizable limit windows".into());
    }
    Ok(metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_weekly_and_monthly_mcp() {
        // Shape from the vendor's community usage scripts: two token
        // windows out of chronological order + the monthly MCP quota.
        let limits = vec![
            serde_json::json!({
                "type": "TOKENS_LIMIT", "percentage": 53,
                "nextResetTime": 1_770_400_000_000i64
            }),
            serde_json::json!({
                "type": "TIME_LIMIT", "percentage": 7, "usage": 1000,
                "currentValue": 72, "remaining": 928,
                "nextResetTime": 1_772_000_000_000i64
            }),
            serde_json::json!({
                "type": "TOKENS_LIMIT", "percentage": 44,
                "nextResetTime": 1_770_000_000_000i64
            }),
        ];
        let m = parse_limits(Some(&limits)).expect("parses");
        assert_eq!(m.len(), 3);
        assert_eq!((m[0].label.as_str(), m[0].used_percent), ("Session", Some(44.0)));
        assert_eq!(m[0].period_ms, Some(5 * HOUR_MS));
        assert_eq!((m[1].label.as_str(), m[1].used_percent), ("Weekly", Some(53.0)));
        assert_eq!(m[2].label.as_str(), "MCP calls");
        assert!((m[2].used_percent.unwrap() - 7.2).abs() < 0.01);
    }

    #[test]
    fn token_window_without_percentage_fails_loudly() {
        let limits = vec![serde_json::json!({ "type": "TOKENS_LIMIT" })];
        assert!(parse_limits(Some(&limits)).is_err());
    }

    #[test]
    fn empty_limits_is_an_error() {
        assert!(parse_limits(Some(&vec![])).is_err());
        assert!(parse_limits(None).is_err());
    }

    #[test]
    fn level_becomes_title_case_plan() {
        let snap = Snapshot::ok("glm_cn", "GLM CN", Some("Pro".into()), vec![]);
        assert_eq!(snap.plan.as_deref(), Some("Pro"));
    }
}
