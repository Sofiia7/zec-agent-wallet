//! The daemon's own database: contacts, bills, approvals, payment operations,
//! payment requests and the event journal. Wallet data lives separately in
//! `wallet.sqlite`, owned by librustzcash.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS contacts (
    id TEXT PRIMARY KEY,
    local_name TEXT NOT NULL,
    alias TEXT NOT NULL,
    delivery_dir TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS contact_addresses (
    contact_id TEXT NOT NULL REFERENCES contacts(id),
    address TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS bills (
    id TEXT PRIMARY KEY,
    uri TEXT NOT NULL,
    uri_hash TEXT NOT NULL UNIQUE,
    address TEXT NOT NULL,
    amount INTEGER NOT NULL,
    memo BLOB,
    ext_label TEXT,
    ext_message TEXT,
    contact_id TEXT REFERENCES contacts(id),
    label TEXT,
    status TEXT NOT NULL,
    source TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS approvals (
    id TEXT PRIMARY KEY,
    bill_id TEXT NOT NULL UNIQUE REFERENCES bills(id),
    snapshot BLOB NOT NULL,
    mac BLOB NOT NULL,
    fee_limit INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS approval_requests (
    bill_id TEXT PRIMARY KEY REFERENCES bills(id),
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    bill_id TEXT NOT NULL UNIQUE REFERENCES bills(id),
    approval_id TEXT NOT NULL REFERENCES approvals(id),
    amount INTEGER NOT NULL,
    reserved_fee INTEGER NOT NULL,
    fee INTEGER,
    state TEXT NOT NULL,
    txid TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS requests (
    id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    address TEXT NOT NULL UNIQUE,
    amount INTEGER NOT NULL,
    label TEXT NOT NULL,
    contact_id TEXT REFERENCES contacts(id),
    uri TEXT NOT NULL,
    delivery TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    ref_id TEXT,
    detail TEXT
);
CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unpredictable identifier with a readable prefix.
pub fn new_id(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &id[..20])
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Contact {
    pub id: String,
    pub local_name: String,
    pub alias: String,
    pub delivery_dir: Option<String>,
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Bill {
    pub id: String,
    pub uri: String,
    pub address: String,
    pub amount: u64,
    pub memo: Option<Vec<u8>>,
    pub ext_label: Option<String>,
    pub ext_message: Option<String>,
    pub contact_id: Option<String>,
    pub label: Option<String>,
    pub status: String,
    pub source: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Approval {
    pub id: String,
    pub bill_id: String,
    #[serde(skip)]
    pub snapshot: Vec<u8>,
    #[serde(skip)]
    pub mac: Vec<u8>,
    pub fee_limit: u64,
    pub expires_at: i64,
    pub status: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Operation {
    pub id: String,
    pub bill_id: String,
    pub approval_id: String,
    pub amount: u64,
    pub reserved_fee: u64,
    pub fee: Option<u64>,
    pub state: String,
    pub txid: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PaymentRequest {
    pub id: String,
    pub idempotency_key: String,
    pub address: String,
    pub amount: u64,
    pub label: String,
    pub contact_id: Option<String>,
    pub uri: String,
    pub delivery: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub ref_id: Option<String>,
    pub detail: Option<String>,
}

/// Outcome of trying to reserve a payment for a bill.
pub enum Reservation {
    New(Operation),
    Existing(Operation),
}

fn bill_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Bill> {
    Ok(Bill {
        id: row.get(0)?,
        uri: row.get(1)?,
        address: row.get(2)?,
        amount: row.get::<_, i64>(3)? as u64,
        memo: row.get(4)?,
        ext_label: row.get(5)?,
        ext_message: row.get(6)?,
        contact_id: row.get(7)?,
        label: row.get(8)?,
        status: row.get(9)?,
        source: row.get(10)?,
        created_at: row.get(11)?,
    })
}

const BILL_COLS: &str =
    "id, uri, address, amount, memo, ext_label, ext_message, contact_id, label, status, source, created_at";

fn approval_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        id: row.get(0)?,
        bill_id: row.get(1)?,
        snapshot: row.get(2)?,
        mac: row.get(3)?,
        fee_limit: row.get::<_, i64>(4)? as u64,
        expires_at: row.get(5)?,
        status: row.get(6)?,
        created_at: row.get(7)?,
    })
}

const APPROVAL_COLS: &str = "id, bill_id, snapshot, mac, fee_limit, expires_at, status, created_at";

fn operation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Operation> {
    Ok(Operation {
        id: row.get(0)?,
        bill_id: row.get(1)?,
        approval_id: row.get(2)?,
        amount: row.get::<_, i64>(3)? as u64,
        reserved_fee: row.get::<_, i64>(4)? as u64,
        fee: row.get::<_, Option<i64>>(5)?.map(|f| f as u64),
        state: row.get(6)?,
        txid: row.get(7)?,
        error: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

const OPERATION_COLS: &str =
    "id, bill_id, approval_id, amount, reserved_fee, fee, state, txid, error, created_at, updated_at";

fn request_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PaymentRequest> {
    Ok(PaymentRequest {
        id: row.get(0)?,
        idempotency_key: row.get(1)?,
        address: row.get(2)?,
        amount: row.get::<_, i64>(3)? as u64,
        label: row.get(4)?,
        contact_id: row.get(5)?,
        uri: row.get(6)?,
        delivery: row.get(7)?,
        created_at: row.get(8)?,
    })
}

const REQUEST_COLS: &str =
    "id, idempotency_key, address, amount, label, contact_id, uri, delivery, created_at";

/// Operation states that still hold a share of the daily limit.
const LIMIT_HOLDING_STATES: &str = "('reserved','building','signed','broadcast_unknown','submitted','mined','confirmed','needs_owner_check')";

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn conn(&self) -> anyhow::Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| anyhow!("store lock poisoned"))
    }

    // ---- key/value settings -------------------------------------------------

    pub fn get_kv(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_kv(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn()?.execute(
            "INSERT INTO kv(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- events -------------------------------------------------------------

    pub fn log(&self, kind: &str, ref_id: Option<&str>, detail: Option<&str>) -> anyhow::Result<()> {
        self.conn()?.execute(
            "INSERT INTO events(at, kind, ref_id, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now(), kind, ref_id, detail],
        )?;
        Ok(())
    }

    pub fn events(&self, limit: u32) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, at, kind, ref_id, detail FROM events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(Event {
                id: r.get(0)?,
                at: r.get(1)?,
                kind: r.get(2)?,
                ref_id: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- contacts -----------------------------------------------------------

    pub fn add_contact(
        &self,
        local_name: &str,
        alias: &str,
        delivery_dir: Option<&str>,
        addresses: &[String],
    ) -> anyhow::Result<String> {
        let id = new_id("ct");
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO contacts(id, local_name, alias, delivery_dir, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, local_name, alias, delivery_dir, now()],
        )?;
        for address in addresses {
            tx.execute(
                "INSERT INTO contact_addresses(contact_id, address) VALUES (?1, ?2)",
                params![id, address],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn contacts(&self) -> anyhow::Result<Vec<Contact>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, local_name, alias, delivery_dir FROM contacts ORDER BY created_at",
        )?;
        let mut contacts: Vec<Contact> = stmt
            .query_map([], |r| {
                Ok(Contact {
                    id: r.get(0)?,
                    local_name: r.get(1)?,
                    alias: r.get(2)?,
                    delivery_dir: r.get(3)?,
                    addresses: vec![],
                })
            })?
            .collect::<Result<_, _>>()?;
        let mut stmt =
            conn.prepare("SELECT address FROM contact_addresses WHERE contact_id = ?1")?;
        for contact in &mut contacts {
            contact.addresses = stmt
                .query_map([&contact.id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
        }
        Ok(contacts)
    }

    pub fn contact(&self, id: &str) -> anyhow::Result<Option<Contact>> {
        Ok(self.contacts()?.into_iter().find(|c| c.id == id))
    }

    pub fn contact_by_address(&self, address: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row(
                "SELECT contact_id FROM contact_addresses WHERE address = ?1",
                [address],
                |r| r.get(0),
            )
            .optional()?)
    }

    // ---- bills --------------------------------------------------------------

    /// Inserts a bill unless the same URI was already imported. Returns the bill id
    /// and whether it is new.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_bill(
        &self,
        uri: &str,
        uri_hash: &str,
        address: &str,
        amount: u64,
        memo: Option<&[u8]>,
        ext_label: Option<&str>,
        ext_message: Option<&str>,
        contact_id: Option<&str>,
        source: &str,
    ) -> anyhow::Result<(String, bool)> {
        let conn = self.conn()?;
        if let Some(id) = conn
            .query_row("SELECT id FROM bills WHERE uri_hash = ?1", [uri_hash], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            return Ok((id, false));
        }
        let id = new_id("bill");
        conn.execute(
            "INSERT INTO bills(id, uri, uri_hash, address, amount, memo, ext_label, ext_message, contact_id, label, status, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, 'needs_approval', ?10, ?11)",
            params![id, uri, uri_hash, address, amount as i64, memo, ext_label, ext_message, contact_id, source, now()],
        )?;
        Ok((id, true))
    }

    pub fn bill(&self, id: &str) -> anyhow::Result<Option<Bill>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {BILL_COLS} FROM bills WHERE id = ?1"),
                [id],
                bill_from_row,
            )
            .optional()?)
    }

    pub fn bills(&self) -> anyhow::Result<Vec<Bill>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare(&format!("SELECT {BILL_COLS} FROM bills ORDER BY created_at DESC"))?;
        let rows = stmt.query_map([], bill_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_bill_status(&self, id: &str, status: &str) -> anyhow::Result<()> {
        self.conn()?
            .execute("UPDATE bills SET status = ?2 WHERE id = ?1", params![id, status])?;
        Ok(())
    }

    pub fn request_approval(&self, bill_id: &str) -> anyhow::Result<()> {
        self.conn()?.execute(
            "INSERT OR IGNORE INTO approval_requests(bill_id, created_at) VALUES (?1, ?2)",
            params![bill_id, now()],
        )?;
        Ok(())
    }

    pub fn approval_requested(&self, bill_id: &str) -> anyhow::Result<bool> {
        Ok(self
            .conn()?
            .query_row(
                "SELECT 1 FROM approval_requests WHERE bill_id = ?1",
                [bill_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn approval_requests_today(&self) -> anyhow::Result<u32> {
        Ok(self.conn()?.query_row(
            "SELECT COUNT(*) FROM approval_requests WHERE created_at > ?1",
            [now() - 86_400],
            |r| r.get(0),
        )?)
    }

    // ---- approvals ----------------------------------------------------------

    /// Records an owner approval of a bill, assigning contact and label at the same time.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_approval(
        &self,
        id: &str,
        bill_id: &str,
        snapshot: &[u8],
        mac: &[u8],
        fee_limit: u64,
        expires_at: i64,
        contact_id: Option<&str>,
        label: &str,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO approvals(id, bill_id, snapshot, mac, fee_limit, expires_at, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
            params![id, bill_id, snapshot, mac, fee_limit as i64, expires_at, now()],
        )?;
        tx.execute(
            "UPDATE bills SET status = 'approved', label = ?2, contact_id = ?3 WHERE id = ?1",
            params![bill_id, label, contact_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn approval_for_bill(&self, bill_id: &str) -> anyhow::Result<Option<Approval>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {APPROVAL_COLS} FROM approvals WHERE bill_id = ?1"),
                [bill_id],
                approval_from_row,
            )
            .optional()?)
    }

    pub fn approval(&self, id: &str) -> anyhow::Result<Option<Approval>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {APPROVAL_COLS} FROM approvals WHERE id = ?1"),
                [id],
                approval_from_row,
            )
            .optional()?)
    }

    pub fn set_approval_status(&self, id: &str, status: &str) -> anyhow::Result<()> {
        self.conn()?.execute(
            "UPDATE approvals SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }

    /// Revokes an approval only if no payment has started from it.
    pub fn revoke_approval(&self, id: &str) -> anyhow::Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE approvals SET status = 'revoked' WHERE id = ?1 AND status = 'active'",
            [id],
        )?;
        if changed == 1 {
            tx.execute(
                "UPDATE bills SET status = 'needs_approval' WHERE id = (SELECT bill_id FROM approvals WHERE id = ?1)",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    // ---- operations ---------------------------------------------------------

    /// Atomically reserves the approval and the daily limit for one payment of a bill.
    /// A second call for the same bill returns the existing operation.
    pub fn reserve_payment(
        &self,
        bill_id: &str,
        approval_id: &str,
        amount: u64,
        reserved_fee: u64,
        daily_limit: u64,
    ) -> anyhow::Result<Result<Reservation, &'static str>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(existing) = tx
            .query_row(
                &format!("SELECT {OPERATION_COLS} FROM operations WHERE bill_id = ?1"),
                [bill_id],
                operation_from_row,
            )
            .optional()?
        {
            return Ok(Ok(Reservation::Existing(existing)));
        }
        let used: i64 = tx.query_row(
            &format!(
                "SELECT COALESCE(SUM(amount + COALESCE(fee, reserved_fee)), 0) FROM operations
                 WHERE created_at > ?1 AND state IN {LIMIT_HOLDING_STATES}"
            ),
            [now() - 86_400],
            |r| r.get(0),
        )?;
        if (used as u64).saturating_add(amount).saturating_add(reserved_fee) > daily_limit {
            return Ok(Err("over_daily_limit"));
        }
        let changed = tx.execute(
            "UPDATE approvals SET status = 'in_use' WHERE id = ?1 AND status = 'active'",
            [approval_id],
        )?;
        if changed != 1 {
            return Ok(Err("not_approved"));
        }
        let id = new_id("op");
        let ts = now();
        tx.execute(
            "INSERT INTO operations(id, bill_id, approval_id, amount, reserved_fee, fee, state, txid, error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'reserved', NULL, NULL, ?6, ?6)",
            params![id, bill_id, approval_id, amount as i64, reserved_fee as i64, ts],
        )?;
        tx.execute("UPDATE bills SET status = 'paying' WHERE id = ?1", [bill_id])?;
        let op = tx.query_row(
            &format!("SELECT {OPERATION_COLS} FROM operations WHERE id = ?1"),
            [&id],
            operation_from_row,
        )?;
        tx.commit()?;
        Ok(Ok(Reservation::New(op)))
    }

    pub fn daily_used(&self) -> anyhow::Result<u64> {
        let used: i64 = self.conn()?.query_row(
            &format!(
                "SELECT COALESCE(SUM(amount + COALESCE(fee, reserved_fee)), 0) FROM operations
                 WHERE created_at > ?1 AND state IN {LIMIT_HOLDING_STATES}"
            ),
            [now() - 86_400],
            |r| r.get(0),
        )?;
        Ok(used as u64)
    }

    pub fn operation(&self, id: &str) -> anyhow::Result<Option<Operation>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {OPERATION_COLS} FROM operations WHERE id = ?1"),
                [id],
                operation_from_row,
            )
            .optional()?)
    }

    pub fn operation_for_bill(&self, bill_id: &str) -> anyhow::Result<Option<Operation>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {OPERATION_COLS} FROM operations WHERE bill_id = ?1"),
                [bill_id],
                operation_from_row,
            )
            .optional()?)
    }

    pub fn operations_in(&self, states: &[&str]) -> anyhow::Result<Vec<Operation>> {
        let conn = self.conn()?;
        let list = states
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT {OPERATION_COLS} FROM operations WHERE state IN ({list}) ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], operation_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    #[cfg(test)]
    pub fn operations(&self) -> anyhow::Result<Vec<Operation>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {OPERATION_COLS} FROM operations ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([], operation_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_operation_state(
        &self,
        id: &str,
        state: &str,
        txid: Option<&str>,
        fee: Option<u64>,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn()?.execute(
            "UPDATE operations SET state = ?2, txid = COALESCE(?3, txid), fee = COALESCE(?4, fee),
             error = ?5, updated_at = ?6 WHERE id = ?1",
            params![id, state, txid, fee.map(|f| f as i64), error, now()],
        )?;
        Ok(())
    }

    // ---- payment requests ---------------------------------------------------

    pub fn request_by_key(&self, key: &str) -> anyhow::Result<Option<PaymentRequest>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {REQUEST_COLS} FROM requests WHERE idempotency_key = ?1"),
                [key],
                request_from_row,
            )
            .optional()?)
    }

    pub fn request(&self, id: &str) -> anyhow::Result<Option<PaymentRequest>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {REQUEST_COLS} FROM requests WHERE id = ?1"),
                [id],
                request_from_row,
            )
            .optional()?)
    }

    pub fn requests(&self) -> anyhow::Result<Vec<PaymentRequest>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {REQUEST_COLS} FROM requests ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([], request_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn open_requests(&self) -> anyhow::Result<u32> {
        Ok(self.conn()?.query_row(
            "SELECT COUNT(*) FROM requests WHERE created_at > ?1",
            [now() - 86_400],
            |r| r.get(0),
        )?)
    }

    pub fn insert_request(&self, req: &PaymentRequest) -> anyhow::Result<()> {
        self.conn()?.execute(
            "INSERT INTO requests(id, idempotency_key, address, amount, label, contact_id, uri, delivery, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                req.id,
                req.idempotency_key,
                req.address,
                req.amount as i64,
                req.label,
                req.contact_id,
                req.uri,
                req.delivery,
                req.created_at
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Store, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("zaw-test-{}.sqlite", uuid::Uuid::new_v4().simple()));
        (Store::open(&path).unwrap(), path)
    }

    fn bill_with_approval(store: &Store, amount: u64) -> (String, String) {
        let (bill, _) = store
            .insert_bill(
                &format!("zcash:x?amount={amount}"),
                &new_id("h"),
                "utest1recipient",
                amount,
                None,
                None,
                None,
                None,
                "owner",
            )
            .unwrap();
        let approval = new_id("ap");
        store
            .insert_approval(&approval, &bill, b"{}", b"mac", 10_000, now() + 3600, None, "Logo")
            .unwrap();
        (bill, approval)
    }

    #[test]
    fn second_reservation_returns_the_same_operation() {
        let (store, _path) = temp_store();
        let (bill, approval) = bill_with_approval(&store, 500_000);
        let first = match store.reserve_payment(&bill, &approval, 500_000, 10_000, 10_000_000).unwrap() {
            Ok(Reservation::New(op)) => op,
            _ => panic!("expected a new operation"),
        };
        match store.reserve_payment(&bill, &approval, 500_000, 10_000, 10_000_000).unwrap() {
            Ok(Reservation::Existing(op)) => assert_eq!(op.id, first.id),
            _ => panic!("expected the existing operation"),
        }
        assert_eq!(store.approval(&approval).unwrap().unwrap().status, "in_use");
    }

    #[test]
    fn concurrent_reservations_create_one_operation() {
        let (store, _path) = temp_store();
        let (bill, approval) = bill_with_approval(&store, 500_000);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let (bill, approval) = (bill.clone(), approval.clone());
                std::thread::spawn(move || {
                    matches!(
                        store.reserve_payment(&bill, &approval, 500_000, 10_000, 10_000_000).unwrap(),
                        Ok(Reservation::New(_))
                    )
                })
            })
            .collect();
        let new_ops = handles.into_iter().filter(|_| true).map(|h| h.join().unwrap()).filter(|n| *n).count();
        assert_eq!(new_ops, 1);
        assert_eq!(store.operations().unwrap().len(), 1);
    }

    #[test]
    fn daily_limit_counts_reservations_and_fees() {
        let (store, _path) = temp_store();
        let (a, ap_a) = bill_with_approval(&store, 600_000);
        let (b, ap_b) = bill_with_approval(&store, 600_000);
        assert!(matches!(
            store.reserve_payment(&a, &ap_a, 600_000, 10_000, 1_000_000).unwrap(),
            Ok(Reservation::New(_))
        ));
        assert!(matches!(
            store.reserve_payment(&b, &ap_b, 600_000, 10_000, 1_000_000).unwrap(),
            Err("over_daily_limit")
        ));
        assert_eq!(store.approval(&ap_b).unwrap().unwrap().status, "active");
    }

    #[test]
    fn zero_limit_pays_nothing() {
        let (store, _path) = temp_store();
        let (a, ap_a) = bill_with_approval(&store, 1);
        assert!(matches!(
            store.reserve_payment(&a, &ap_a, 1, 0, 0).unwrap(),
            Err("over_daily_limit")
        ));
    }

    #[test]
    fn revoke_only_before_payment_starts() {
        let (store, _path) = temp_store();
        let (a, ap_a) = bill_with_approval(&store, 1_000);
        let (b, ap_b) = bill_with_approval(&store, 1_000);
        assert!(store.revoke_approval(&ap_a).unwrap());
        assert!(matches!(
            store.reserve_payment(&a, &ap_a, 1_000, 10, 1_000_000).unwrap(),
            Err("not_approved")
        ));
        store.reserve_payment(&b, &ap_b, 1_000, 10, 1_000_000).unwrap().ok();
        assert!(!store.revoke_approval(&ap_b).unwrap());
    }

    #[test]
    fn same_link_imports_once() {
        let (store, _path) = temp_store();
        let first = store
            .insert_bill("zcash:x", "hash-1", "utest1r", 5, None, None, None, None, "inbox")
            .unwrap();
        let second = store
            .insert_bill("zcash:x", "hash-1", "utest1r", 5, None, None, None, None, "inbox")
            .unwrap();
        assert!(first.1);
        assert!(!second.1);
        assert_eq!(first.0, second.0);
    }
}
