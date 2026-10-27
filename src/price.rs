//! The ZEC/USD price, for showing approximate dollar amounts. Off until the owner
//! turns it on, and never on testnet. While on, the price is fetched on a fixed
//! timer, never around a payment: the price service learns the wallet's IP address
//! and that it is running, not when it pays.

use std::time::Duration;

use anyhow::anyhow;
use serde_json::Value;

/// How often the price is fetched while USD amounts are on.
pub const REFRESH: Duration = Duration::from_secs(600);
/// A price older than this is not shown.
pub const MAX_AGE_SECS: i64 = 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub micro_usd_per_zec: u64,
    pub at: i64,
    pub source: &'static str,
}

const SOURCES: [(&str, &str); 2] = [
    ("Coinbase", "https://api.coinbase.com/v2/prices/ZEC-USD/spot"),
    ("Kraken", "https://api.kraken.com/0/public/Ticker?pair=ZECUSD"),
];

pub fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("zaw")
        .build()?)
}

/// Asks Coinbase, then Kraken. Returns micro-dollars per ZEC and the source.
pub async fn fetch(http: &reqwest::Client) -> anyhow::Result<(u64, &'static str)> {
    let mut last = anyhow!("no price source answered");
    for (source, url) in SOURCES {
        let body: anyhow::Result<Value> = async {
            let res = http.get(url).send().await?.error_for_status()?;
            Ok(res.json().await?)
        }
        .await;
        match body.map(|v| parse(source, &v)) {
            Ok(Some(price)) => return Ok((price, source)),
            Ok(None) => last = anyhow!("{source}: unexpected answer"),
            Err(e) => last = anyhow!("{source}: {e}"),
        }
    }
    Err(last)
}

fn parse(source: &str, v: &Value) -> Option<u64> {
    let text = match source {
        "Coinbase" => v["data"]["amount"].as_str(),
        _ => v["result"].as_object()?.values().next()?["c"][0].as_str(),
    }?;
    parse_usd(text)
}

/// "40.1234" -> 40_123_400 micro-dollars. Digits past the sixth decimal are dropped.
/// Anything that is not a plain positive decimal, or is implausible for ZEC, is refused.
pub fn parse_usd(s: &str) -> Option<u64> {
    let (whole, frac) = s.trim().split_once('.').unwrap_or((s.trim(), ""));
    if whole.is_empty()
        || whole.len() > 7
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let frac: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
    let micro = whole.parse::<u64>().ok()? * 1_000_000 + frac.parse::<u64>().ok()?;
    (micro > 0 && micro < 1_000_000 * 1_000_000).then_some(micro)
}

/// Dollar value of `zat` zatoshis, rounded to cents: "12.34", or "<0.01" for dust.
pub fn usd_text(zat: u64, micro_usd_per_zec: u64) -> String {
    let micro = zat as u128 * micro_usd_per_zec as u128 / 100_000_000;
    let cents = (micro + 5_000) / 10_000;
    if cents == 0 && micro > 0 {
        return "<0.01".to_string();
    }
    format!("{}.{:02}", cents / 100, cents % 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_both_sources() {
        let coinbase = json!({ "data": { "amount": "40.125", "base": "ZEC", "currency": "USD" } });
        assert_eq!(parse("Coinbase", &coinbase), Some(40_125_000));
        let kraken = json!({ "error": [], "result": { "XZECZUSD": { "c": ["39.98000", "1.2"] } } });
        assert_eq!(parse("Kraken", &kraken), Some(39_980_000));
        assert_eq!(parse("Coinbase", &json!({ "errors": [] })), None);
    }

    #[test]
    fn refuses_odd_prices() {
        for bad in ["", ".5", "-1", "1e3", "0", "0.0000001", "abc", "12345678"] {
            assert_eq!(parse_usd(bad), None, "{bad}");
        }
        assert_eq!(parse_usd("40.1234567"), Some(40_123_456));
    }

    #[test]
    fn dollar_text() {
        let price = 40_000_000; // $40 per ZEC
        assert_eq!(usd_text(100_000, price), "0.04"); // 0.001 ZEC
        assert_eq!(usd_text(10_000, price), "<0.01"); // 0.0001 ZEC
        assert_eq!(usd_text(0, price), "0.00");
        assert_eq!(usd_text(250_000_000, price), "100.00");
        assert_eq!(usd_text(12_345, price), "<0.01");
        assert_eq!(usd_text(13_000, price), "0.01");
    }
}
