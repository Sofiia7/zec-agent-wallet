//! Everything the model can ever see. These types are the only ones the agent API
//! and the MCP server serialise; they have no fields for addresses, memos, labels or
//! messages written by counterparties, transaction ids, file paths or keys.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::policy::Reason;

#[derive(Clone, Debug, Serialize)]
pub struct Amount {
    pub zec: String,
    /// Approximate, for scale only. Absent on testnet, where coins have no price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_approx: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    Wait,
    OwnerReview,
    RetryLater,
    None,
}

#[derive(Clone, Debug, Serialize)]
pub struct WalletStatus {
    pub network: &'static str,
    pub synced: bool,
    pub blocks_behind: u32,
    pub unlocked: bool,
    pub payments_paused: bool,
    pub spendable: Amount,
    pub pending: Amount,
    pub daily_limit_remaining: Amount,
    pub bills_waiting_for_owner: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Contact {
    pub contact_id: String,
    pub alias: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Bill {
    pub bill_id: String,
    /// The contact's alias, or "unknown payee".
    pub payee: String,
    /// Set by the owner when approving; never taken from the counterparty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub amount: Amount,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_until: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovalRequested {
    pub bill_id: String,
    pub state: &'static str,
    pub next_action: NextAction,
}

#[derive(Clone, Debug, Serialize)]
pub struct Payment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub bill_id: String,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmations: Option<u32>,
    pub next_action: NextAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Request {
    pub request_id: String,
    pub label: String,
    pub amount: Amount,
    /// "delivered_to_folder" or "owner_will_send".
    pub delivery: &'static str,
    pub state: &'static str,
    pub detected: Amount,
    pub confirmed: Amount,
    pub confirmations: u32,
    pub next_action: NextAction,
}

#[derive(Clone, Debug, Serialize)]
pub struct Activity {
    pub at: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Error {
    pub reason_code: Reason,
    pub next_action: NextAction,
}

pub fn rfc3339(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

static LEAK_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Bech32 / Bech32m: unified addresses and keys, Sapling, TEX.
        r"(?i)\b(u|utest|uview|uviewtest|uivk|uivktest|zs|ztestsapling|zxviews|zxviewtestsapling|zivks|zivktestsapling|secret-extended-key-main|secret-extended-key-test|tex|textest)1[02-9ac-hj-np-z]{20,}",
        // Base58 transparent addresses.
        r"\b(t1|t3|tm|t2)[1-9A-HJ-NP-Za-km-z]{30,}",
        // Transaction ids and other 32-byte hex values.
        r"\b[0-9a-fA-F]{64}\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid pattern"))
    .collect()
});

/// Last line of defence: true if any string in `value` looks like an address, key or
/// txid. The typed DTOs above are the real boundary; this catches mistakes.
pub fn leaks(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => LEAK_PATTERNS.iter().any(|re| re.is_match(s)),
        serde_json::Value::Array(items) => items.iter().any(leaks),
        serde_json::Value::Object(map) => map.iter().any(|(k, v)| {
            LEAK_PATTERNS.iter().any(|re| re.is_match(k)) || leaks(v)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catches_addresses_and_txids() {
        let ua = "utest1s32cjeujn8xj3jy8qemc00xqkw8cps0kxz90earmpl533475y50qg5ams0m9lsnvg2vjc9y0z94aysv05y2dvwsjy0p58mvejyx8awjn";
        assert!(leaks(&json!({ "x": ua })));
        assert!(leaks(&json!(["tmBsTi2xWTjUdEXnuTceL7fecEQKeWaPDJd"])));
        assert!(leaks(&json!({
            "t": "06e5a29ea559c4249ba994c8c77f24544a7a02271e581b6db55c9c561f9bfc60"
        })));
    }

    #[test]
    fn allows_normal_responses() {
        let ok = json!({
            "bill_id": "bill_0123456789abcdef0123",
            "payee": "Designer",
            "amount": { "zec": "0.005" },
            "status": "approved",
            "label": "Logo #17 for the team"
        });
        assert!(!leaks(&ok));
    }
}
