//! Owner approvals: an approval is a signed snapshot of exactly one payment.
//!
//! The snapshot is MACed with a key derived from the seed, which exists only in
//! daemon memory while the wallet is unlocked. Editing the database cannot create
//! or widen an approval: the MAC would not verify.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::store::Bill;
use crate::wallet::Net;

type HmacSha256 = Hmac<Sha256>;

pub const SNAPSHOT_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub v: u8,
    pub approval_id: String,
    pub network: Net,
    pub bill_id: String,
    pub address: String,
    pub amount: u64,
    pub memo_hex: Option<String>,
    pub fee_limit: u64,
    pub expires_at: i64,
    pub uses: u8,
    pub contact_id: Option<String>,
    pub label: String,
}

impl Snapshot {
    pub fn for_bill(
        approval_id: &str,
        network: Net,
        bill: &Bill,
        fee_limit: u64,
        expires_at: i64,
        contact_id: Option<String>,
        label: String,
    ) -> Self {
        Snapshot {
            v: SNAPSHOT_VERSION,
            approval_id: approval_id.to_string(),
            network,
            bill_id: bill.id.clone(),
            address: bill.address.clone(),
            amount: bill.amount,
            memo_hex: bill.memo.as_ref().map(hex::encode),
            fee_limit,
            expires_at,
            uses: 1,
            contact_id,
            label,
        }
    }

    /// Canonical bytes: serde_json writes struct fields in declaration order.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("snapshot serialises")
    }

    /// True when the snapshot still describes this bill exactly.
    pub fn matches(&self, network: Net, bill: &Bill) -> bool {
        self.v == SNAPSHOT_VERSION
            && self.network == network
            && self.bill_id == bill.id
            && self.address == bill.address
            && self.amount == bill.amount
            && self.memo_hex == bill.memo.as_ref().map(hex::encode)
            && self.uses == 1
    }
}

pub fn sign(key: &[u8; 32], bytes: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("any key length works");
    mac.update(bytes);
    mac.finalize().into_bytes().to_vec()
}

pub fn verify(key: &[u8; 32], bytes: &[u8], tag: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("any key length works");
    mac.update(bytes);
    mac.verify_slice(tag).is_ok()
}

/// Reasons the model may see. A fixed list: nothing from counterparties or from
/// library errors is ever forwarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    NotFound,
    NotApproved,
    ApprovalPending,
    ApprovalExpired,
    ApprovalRevoked,
    AlreadyPaid,
    ApprovalMismatch,
    UnsupportedRecipient,
    WrongNetwork,
    OverFeeLimit,
    OverDailyLimit,
    InsufficientFunds,
    NotSynced,
    WalletLocked,
    PaymentsPaused,
    InvalidAmount,
    TooManyRequests,
    InternalError,
}

impl Reason {
    pub fn from_code(code: &str) -> Reason {
        match code {
            "over_daily_limit" => Reason::OverDailyLimit,
            "not_approved" => Reason::NotApproved,
            _ => Reason::InternalError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bill() -> Bill {
        Bill {
            id: "bill_1".into(),
            uri: "zcash:...".into(),
            address: "utest1example".into(),
            amount: 500_000,
            memo: Some(vec![1, 2, 3]),
            ext_label: None,
            ext_message: None,
            contact_id: None,
            label: None,
            status: "needs_approval".into(),
            source: "owner".into(),
            created_at: 0,
        }
    }

    #[test]
    fn mac_detects_any_change() {
        let key = [7u8; 32];
        let snap = Snapshot::for_bill("ap_1", Net::Testnet, &bill(), 50_000, 10, None, "Logo".into());
        let tag = sign(&key, &snap.to_bytes());
        assert!(verify(&key, &snap.to_bytes(), &tag));

        let mut wider = snap.clone();
        wider.amount = 5_000_000;
        assert!(!verify(&key, &wider.to_bytes(), &tag));
        assert!(!verify(&[8u8; 32], &snap.to_bytes(), &tag));
    }

    #[test]
    fn snapshot_must_match_bill() {
        let snap = Snapshot::for_bill("ap_1", Net::Testnet, &bill(), 50_000, 10, None, "Logo".into());
        assert!(snap.matches(Net::Testnet, &bill()));
        let mut other = bill();
        other.address = "utest1attacker".into();
        assert!(!snap.matches(Net::Testnet, &other));
        let mut other = bill();
        other.memo = None;
        assert!(!snap.matches(Net::Testnet, &other));
        assert!(!snap.matches(Net::Mainnet, &bill()));
    }
}
