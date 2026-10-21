//! ZIP 321 payment requests: strict parsing of incoming bills and building our own.

use sha2::{Digest, Sha256};
use zcash_address::ZcashAddress;
use zcash_protocol::value::Zatoshis;
use zip321::{Payment, TransactionRequest};

use crate::wallet::{RecipientError, Wallet};

/// Longest payment URI we accept. A single-recipient request with a full memo fits
/// comfortably.
pub const MAX_URI_LEN: usize = 2048;

#[derive(Debug)]
pub struct ParsedBill {
    pub uri: String,
    pub uri_hash: String,
    pub address: String,
    pub amount: u64,
    pub memo: Option<Vec<u8>>,
    pub label: Option<String>,
    pub message: Option<String>,
}

/// Why a payment URI was not imported. Shown to the owner, never to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillError {
    TooLong,
    Malformed,
    MultipleRecipients,
    NoAmount,
    ZeroAmount,
    UnknownRequiredParam,
    UnsupportedRecipient,
    WrongNetwork,
}

impl BillError {
    pub fn describe(self) -> &'static str {
        match self {
            BillError::TooLong => "the payment link is too long",
            BillError::Malformed => "this is not a valid ZIP 321 payment link",
            BillError::MultipleRecipients => {
                "links with several recipients are not supported; nothing was imported"
            }
            BillError::NoAmount => "the link has no amount",
            BillError::ZeroAmount => "the amount must be positive",
            BillError::UnknownRequiredParam => "the link has a required parameter this wallet does not understand",
            BillError::UnsupportedRecipient => {
                "the recipient must be a unified address with an Orchard receiver (no transparent, TEX or Sapling-only addresses)"
            }
            BillError::WrongNetwork => "the address belongs to another network",
        }
    }
}

pub fn uri_hash(uri: &str) -> String {
    hex::encode(Sha256::digest(uri.trim().as_bytes()))
}

pub fn parse_bill(wallet: &Wallet, uri: &str) -> Result<ParsedBill, BillError> {
    let uri = uri.trim();
    if uri.len() > MAX_URI_LEN {
        return Err(BillError::TooLong);
    }
    let request = TransactionRequest::from_uri(uri).map_err(|_| BillError::Malformed)?;
    let payments = request.payments();
    if payments.len() != 1 {
        return Err(BillError::MultipleRecipients);
    }
    let payment = payments.values().next().ok_or(BillError::Malformed)?;
    if payment
        .other_params()
        .iter()
        .any(|(key, _)| key.starts_with("req-"))
    {
        return Err(BillError::UnknownRequiredParam);
    }
    let amount = payment.amount().ok_or(BillError::NoAmount)?.into_u64();
    if amount == 0 {
        return Err(BillError::ZeroAmount);
    }
    let address = payment.recipient_address().encode();
    wallet.check_recipient(&address).map_err(|e| match e {
        RecipientError::WrongNetwork => BillError::WrongNetwork,
        RecipientError::Unsupported | RecipientError::Invalid => BillError::UnsupportedRecipient,
    })?;
    Ok(ParsedBill {
        uri: uri.to_string(),
        uri_hash: uri_hash(uri),
        address,
        amount,
        memo: payment.memo().map(|m| m.as_slice().to_vec()),
        label: payment.label().cloned(),
        message: payment.message().cloned(),
    })
}

/// Builds a single-recipient payment link for one of our own requests.
pub fn build_request_uri(address: &str, amount: u64, label: &str) -> anyhow::Result<String> {
    let recipient: ZcashAddress = address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid address: {e}"))?;
    let amount = Zatoshis::from_u64(amount).map_err(|_| anyhow::anyhow!("invalid amount"))?;
    let payment = Payment::new(
        recipient,
        Some(amount),
        None,
        Some(label.to_string()),
        None,
        vec![],
    )
    .map_err(|e| anyhow::anyhow!("invalid payment: {e:?}"))?;
    Ok(TransactionRequest::new(vec![payment])
        .map_err(|e| anyhow::anyhow!("invalid request: {e}"))?
        .to_uri())
}

/// Formats zatoshis as a ZEC decimal string without trailing zeros.
pub fn format_zec(zat: u64) -> String {
    let whole = zat / 100_000_000;
    let frac = zat % 100_000_000;
    if frac == 0 {
        return format!("{whole}");
    }
    let frac = format!("{frac:08}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// Parses a positive ZEC decimal string ("0.005") into zatoshis, strictly.
pub fn parse_zec(s: &str) -> Option<u64> {
    let s = s.trim();
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    if frac.len() > 8
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let frac: u64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<8}").parse().ok()?
    };
    let zat = whole.checked_mul(100_000_000)?.checked_add(frac)?;
    (zat > 0 && zat <= 21_000_000 * 100_000_000).then_some(zat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zec_round_trip() {
        assert_eq!(parse_zec("0.005"), Some(500_000));
        assert_eq!(parse_zec("1"), Some(100_000_000));
        assert_eq!(parse_zec(".5"), Some(50_000_000));
        assert_eq!(format_zec(500_000), "0.005");
        assert_eq!(format_zec(100_000_000), "1");
        assert_eq!(format_zec(123), "0.00000123");
    }

    #[test]
    fn zec_rejects_bad_input() {
        for bad in ["", ".", "0", "-1", "1.123456789", "1e5", "abc", "1.2.3", "21000001"] {
            assert_eq!(parse_zec(bad), None, "{bad}");
        }
    }
}
