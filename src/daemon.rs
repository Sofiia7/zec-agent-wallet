//! The wallet daemon: background sync, the payment executor, the agent API and the
//! owner API. The agent API exposes only `dto` types and the same rules as MCP.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;

pub mod playground;

use crate::dto::{self, NextAction};
use crate::invoice::{self, format_zec, parse_zec};
use crate::policy::{self, Reason, Snapshot};
use crate::store::{self, Bill, Operation, Reservation, Store, new_id, now};
use crate::wallet::{BuildRefusal, Net, RecipientError, SyncStatus, TxStatus, UnlockedKeys, Wallet};

/// Confirmations after which an outgoing payment or an incoming request counts as done.
const DONE_CONFIRMATIONS: u32 = 3;
/// The wallet counts as synced when it is at most this many blocks behind.
const MAX_BLOCKS_BEHIND: u32 = 2;
const SYNC_INTERVAL: Duration = Duration::from_secs(30);
const EXECUTOR_TICK: Duration = Duration::from_secs(10);
const INBOX_MAX_FILE: u64 = 4096;
const MAX_LABEL_LEN: usize = 80;
const MAX_OPEN_REQUESTS_PER_DAY: u32 = 50;
const MAX_APPROVAL_REQUESTS_PER_DAY: u32 = 50;
const MAX_ACTIVITY: u32 = 50;
const SESSION_TTL_SECS: i64 = 12 * 3600;
const DEFAULT_FEE_LIMIT: u64 = 50_000;
const DEFAULT_APPROVAL_HOURS: i64 = 7 * 24;

#[derive(Clone)]
struct Session {
    csrf: String,
    expires_at: i64,
}

pub struct Daemon {
    wallet: Arc<Wallet>,
    store: Store,
    net: Net,
    dir: PathBuf,
    /// Serialises all writes to the librustzcash database (sync, address generation,
    /// transaction building).
    wallet_lock: tokio::sync::Mutex<()>,
    keys: Mutex<Option<Arc<UnlockedKeys>>>,
    sessions: Mutex<HashMap<String, Session>>,
    agent_token: String,
    notify: Notify,
    last_sync: Mutex<Option<SyncStatus>>,
    allowed_hosts: Vec<String>,
    /// Set when served over HTTPS behind a reverse proxy.
    secure_cookies: bool,
    playground: Option<playground::Playground>,
}

pub struct Options {
    pub listen: String,
    pub public_host: Option<String>,
    pub unlock_password: Option<String>,
    pub playground: Option<playground::Config>,
}

fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Reads the agent token, creating it on first start. The MCP server reads the same
/// file; the owner API never accepts it.
pub fn agent_token(dir: &Path) -> anyhow::Result<String> {
    let path = dir.join("agent-token");
    if let Ok(token) = std::fs::read_to_string(&path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = random_token();
    std::fs::write(&path, &token).context("writing agent token")?;
    Ok(token)
}

pub async fn run(dir: &Path, opts: Options) -> anyhow::Result<()> {
    let wallet = Arc::new(Wallet::open(dir)?);
    let net = wallet.config().network;
    let store = Store::open(&dir.join("zaw.sqlite"))?;
    for sub in ["inbox", "inbox/processed", "inbox/rejected"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let port = opts
        .listen
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut allowed_hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if let Some(host) = &opts.public_host {
        allowed_hosts.push(host.clone());
    }

    let daemon = Arc::new(Daemon {
        wallet,
        store,
        net,
        dir: dir.to_path_buf(),
        wallet_lock: tokio::sync::Mutex::new(()),
        keys: Mutex::new(None),
        sessions: Mutex::new(HashMap::new()),
        agent_token: agent_token(dir)?,
        notify: Notify::new(),
        last_sync: Mutex::new(None),
        allowed_hosts,
        secure_cookies: opts.public_host.is_some(),
        playground: opts.playground.map(playground::Playground::new).transpose()?,
    });

    daemon.recover()?;
    if let Some(password) = opts.unlock_password {
        let keys = daemon.unlock(password).await?;
        *daemon.keys.lock().map_err(|_| anyhow!("lock"))? = Some(keys);
        tracing::info!("wallet unlocked from environment");
    }

    tokio::spawn(daemon.clone().sync_loop());
    tokio::spawn(daemon.clone().executor_loop());

    let app = router(daemon.clone());
    let listener = tokio::net::TcpListener::bind(&opts.listen)
        .await
        .with_context(|| format!("binding {}", opts.listen))?;
    println!("zaw daemon on http://{} ({:?})", opts.listen, net);
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

impl Daemon {
    fn keys(&self) -> Option<Arc<UnlockedKeys>> {
        self.keys.lock().ok().and_then(|k| k.clone())
    }

    fn paused(&self) -> bool {
        self.store.get_kv("paused").ok().flatten().as_deref() == Some("1")
    }

    fn daily_limit(&self) -> u64 {
        self.store
            .get_kv("daily_limit")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn sync_status(&self) -> Option<SyncStatus> {
        self.last_sync.lock().ok().and_then(|s| *s)
    }

    async fn unlock(&self, password: String) -> anyhow::Result<Arc<UnlockedKeys>> {
        let wallet = self.wallet.clone();
        let keys = tokio::task::spawn_blocking(move || wallet.unlock(&password)).await??;
        Ok(Arc::new(keys))
    }

    /// Operations interrupted while building may or may not have produced a
    /// transaction. They are never retried automatically.
    fn recover(&self) -> anyhow::Result<()> {
        for op in self.store.operations_in(&["building"])? {
            self.store.set_operation_state(
                &op.id,
                "needs_owner_check",
                None,
                None,
                Some("interrupted while building"),
            )?;
            self.store
                .log("payment_needs_owner_check", Some(&op.id), None)?;
        }
        Ok(())
    }

    async fn sync_once(&self) -> anyhow::Result<()> {
        let _guard = self.wallet_lock.lock().await;
        let status = self.wallet.sync().await?;
        if let Ok(mut last) = self.last_sync.lock() {
            *last = Some(status);
        }
        Ok(())
    }

    async fn sync_loop(self: Arc<Self>) {
        loop {
            if let Err(e) = self.sync_once().await {
                tracing::warn!("sync failed: {e:#}");
            }
            if let Err(e) = self.scan_inbox() {
                tracing::warn!("inbox scan failed: {e:#}");
            }
            self.notify.notify_one();
            tokio::time::sleep(SYNC_INTERVAL).await;
        }
    }

    async fn executor_loop(self: Arc<Self>) {
        loop {
            if let Err(e) = self.execute_pending().await {
                tracing::warn!("payment executor: {e:#}");
            }
            let _ = tokio::time::timeout(EXECUTOR_TICK, self.notify.notified()).await;
        }
    }

    async fn execute_pending(&self) -> anyhow::Result<()> {
        for op in self.store.operations_in(&["reserved"])? {
            self.build_operation(op).await?;
        }
        for op in self.store.operations_in(&["signed", "broadcast_unknown"])? {
            self.broadcast_operation(&op).await?;
        }
        for op in self.store.operations_in(&["submitted", "mined", "broadcast_unknown"])? {
            self.track_operation(&op)?;
        }
        Ok(())
    }

    fn fail_before_signing(&self, op: &Operation, reason: Reason) -> anyhow::Result<()> {
        let code = serde_json::to_value(reason)?
            .as_str()
            .unwrap_or("internal_error")
            .to_string();
        self.store
            .set_operation_state(&op.id, "failed_before_signing", None, None, Some(&code))?;
        self.store.set_bill_status(&op.bill_id, "failed")?;
        self.store.set_approval_status(&op.approval_id, "failed")?;
        self.store.log("payment_failed", Some(&op.id), Some(&code))?;
        Ok(())
    }

    /// Builds and signs one reserved payment. The approval is verified again right
    /// before signing; nothing from the agent's request is trusted here.
    async fn build_operation(&self, op: Operation) -> anyhow::Result<()> {
        if self.paused() {
            return Ok(());
        }
        let Some(keys) = self.keys() else {
            return Ok(());
        };
        let (Some(bill), Some(approval)) = (
            self.store.bill(&op.bill_id)?,
            self.store.approval(&op.approval_id)?,
        ) else {
            return self.fail_before_signing(&op, Reason::NotApproved);
        };
        if approval.status != "in_use"
            || !policy::verify(&keys.approval_key, &approval.snapshot, &approval.mac)
        {
            return self.fail_before_signing(&op, Reason::ApprovalMismatch);
        }
        let snapshot: Snapshot = serde_json::from_slice(&approval.snapshot)?;
        if !snapshot.matches(self.net, &bill) || snapshot.amount != op.amount {
            return self.fail_before_signing(&op, Reason::ApprovalMismatch);
        }
        if snapshot.expires_at < now() {
            return self.fail_before_signing(&op, Reason::ApprovalExpired);
        }

        let _guard = self.wallet_lock.lock().await;
        if let Err(e) = self.wallet.sync().await {
            tracing::warn!("sync before payment failed, will retry: {e:#}");
            return Ok(());
        }
        self.store
            .set_operation_state(&op.id, "building", None, None, None)?;
        let memo = match &bill.memo {
            Some(bytes) => Some(MemoBytes::from_bytes(bytes).map_err(|e| anyhow!("memo: {e:?}"))?),
            None => None,
        };
        let amount = Zatoshis::from_u64(bill.amount).map_err(|_| anyhow!("amount"))?;
        let wallet = self.wallet.clone();
        let address = bill.address.clone();
        let fee_limit = snapshot.fee_limit;
        let built = tokio::task::spawn_blocking(move || {
            wallet.build_payment(&keys.usk, &address, amount, memo, fee_limit)
        })
        .await?;
        match built {
            Ok(Ok((txid, fee))) => {
                let txid = txid.to_string();
                self.store
                    .set_operation_state(&op.id, "signed", Some(&txid), Some(fee), None)?;
                self.store.set_approval_status(&op.approval_id, "used")?;
                self.store.log("payment_signed", Some(&op.id), None)?;
            }
            Ok(Err(BuildRefusal::FeeTooHigh(_))) => {
                self.fail_before_signing(&op, Reason::OverFeeLimit)?
            }
            Ok(Err(BuildRefusal::InsufficientFunds)) => {
                self.fail_before_signing(&op, Reason::InsufficientFunds)?
            }
            Err(e) => {
                tracing::warn!("building payment failed: {e:#}");
                self.fail_before_signing(&op, Reason::InternalError)?
            }
        }
        Ok(())
    }

    /// Sends the stored transaction. Repeating it sends the same bytes.
    async fn broadcast_operation(&self, op: &Operation) -> anyhow::Result<()> {
        let Some(txid) = &op.txid else {
            return Ok(());
        };
        let txid = crate::wallet::parse_txid(txid)?;
        match self.wallet.broadcast(txid).await {
            Ok(()) => {
                self.store
                    .set_operation_state(&op.id, "submitted", None, None, None)?;
                self.store.set_bill_status(&op.bill_id, "paying")?;
                self.store.log("payment_submitted", Some(&op.id), None)?;
            }
            Err(e) => {
                tracing::warn!("broadcast failed, will check the chain: {e:#}");
                if op.state != "broadcast_unknown" {
                    self.store.set_operation_state(
                        &op.id,
                        "broadcast_unknown",
                        None,
                        None,
                        Some("broadcast result unknown"),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn track_operation(&self, op: &Operation) -> anyhow::Result<()> {
        let Some(txid) = &op.txid else {
            return Ok(());
        };
        let txid = crate::wallet::parse_txid(txid)?;
        match self.wallet.tx_status(txid)? {
            TxStatus::Mined { confirmations, .. } if confirmations >= DONE_CONFIRMATIONS => {
                self.store
                    .set_operation_state(&op.id, "confirmed", None, None, None)?;
                self.store.set_bill_status(&op.bill_id, "paid")?;
                self.store.log("payment_confirmed", Some(&op.id), None)?;
            }
            TxStatus::Mined { .. } => {
                if op.state != "mined" {
                    self.store
                        .set_operation_state(&op.id, "mined", None, None, None)?;
                }
            }
            TxStatus::Unmined => {
                let expiry = self.wallet.tx_expiry(txid)?;
                let tip = self.sync_status().map(|s| s.chain_tip);
                if let (Some(expiry), Some(tip)) = (expiry, tip) {
                    if expiry != 0 && tip > expiry {
                        self.store.set_operation_state(
                            &op.id,
                            "expired_unpaid",
                            None,
                            None,
                            Some("transaction expired before it was mined"),
                        )?;
                        self.store.set_bill_status(&op.bill_id, "expired_unpaid")?;
                        self.store.log("payment_expired", Some(&op.id), None)?;
                    }
                }
            }
            TxStatus::Unknown => {}
        }
        Ok(())
    }

    fn import_bill(&self, uri: &str, source: &str) -> Result<(String, bool), invoice::BillError> {
        let parsed = invoice::parse_bill(&self.wallet, uri)?;
        let contact = self
            .store
            .contact_by_address(&parsed.address)
            .ok()
            .flatten();
        let (id, new) = self
            .store
            .insert_bill(
                &parsed.uri,
                &parsed.uri_hash,
                &parsed.address,
                parsed.amount,
                parsed.memo.as_deref(),
                parsed.label.as_deref(),
                parsed.message.as_deref(),
                contact.as_deref(),
                source,
            )
            .map_err(|_| invoice::BillError::Malformed)?;
        if new {
            let _ = self.store.log("bill_imported", Some(&id), Some(source));
        }
        Ok((id, new))
    }

    /// Imports payment links dropped into `inbox/`. Only regular `.txt` files are read;
    /// symlinks and oversized files are moved to `rejected/`.
    fn scan_inbox(&self) -> anyhow::Result<()> {
        let inbox = self.dir.join("inbox");
        for entry in std::fs::read_dir(&inbox)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let is_txt = path.extension().and_then(|e| e.to_str()) == Some("txt");
            let accepted = is_txt
                && meta.file_type().is_file()
                && meta.len() <= INBOX_MAX_FILE
                && std::fs::read_to_string(&path)
                    .ok()
                    .map(|uri| self.import_bill(&uri, "inbox").is_ok())
                    .unwrap_or(false);
            let target = inbox
                .join(if accepted { "processed" } else { "rejected" })
                .join(&name);
            if std::fs::rename(&path, &target).is_err() {
                let _ = std::fs::remove_file(&path);
            }
            if !accepted {
                self.store.log("inbox_file_rejected", None, None)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Agent-side operations: everything returned here is a `dto` type.
// ---------------------------------------------------------------------------

impl Daemon {
    fn amount(&self, zat: u64) -> dto::Amount {
        dto::Amount {
            zec: format_zec(zat),
            usd_approx: None,
        }
    }

    fn alias(&self, contact_id: Option<&str>) -> String {
        contact_id
            .and_then(|id| self.store.contact(id).ok().flatten())
            .map(|c| c.alias)
            .unwrap_or_else(|| "unknown payee".to_string())
    }

    fn bill_status(&self, bill: &Bill) -> &'static str {
        match bill.status.as_str() {
            "needs_approval" => {
                if self.store.approval_requested(&bill.id).unwrap_or(false) {
                    "approval_requested"
                } else {
                    "needs_approval"
                }
            }
            "approved" => "approved",
            "paying" => "paying",
            "paid" => "paid",
            "rejected" => "rejected",
            "expired_unpaid" => "expired_unpaid",
            _ => "failed",
        }
    }

    pub fn bill_dto(&self, bill: &Bill) -> dto::Bill {
        let approved_until = self
            .store
            .approval_for_bill(&bill.id)
            .ok()
            .flatten()
            .filter(|a| a.status == "active")
            .map(|a| dto::rfc3339(a.expires_at));
        dto::Bill {
            bill_id: bill.id.clone(),
            payee: self.alias(bill.contact_id.as_deref()),
            label: bill.label.clone(),
            amount: self.amount(bill.amount),
            status: self.bill_status(bill),
            approved_until,
        }
    }

    fn op_state(state: &str) -> &'static str {
        match state {
            "reserved" | "building" => "preparing",
            "signed" => "signed",
            "broadcast_unknown" => "sending",
            "submitted" => "submitted",
            "mined" => "in_block",
            "confirmed" => "confirmed",
            "failed_before_signing" => "failed",
            "expired_unpaid" => "expired_unpaid",
            _ => "owner_check",
        }
    }

    fn payment_dto(&self, op: &Operation) -> dto::Payment {
        let confirmations = op.txid.as_ref().and_then(|txid| {
            crate::wallet::parse_txid(txid)
                .ok()
                .and_then(|t| self.wallet.tx_status(t).ok())
                .and_then(|s| match s {
                    TxStatus::Mined { confirmations, .. } => Some(confirmations),
                    _ => None,
                })
        });
        let state = Self::op_state(&op.state);
        let next_action = match state {
            "confirmed" => NextAction::None,
            "failed" | "expired_unpaid" | "owner_check" => NextAction::OwnerReview,
            _ => NextAction::Wait,
        };
        let reason_code = if op.state == "failed_before_signing" {
            op.error
                .as_deref()
                .and_then(|code| serde_json::from_value(json!(code)).ok())
                .map(|r: ReasonCode| r.0)
        } else {
            None
        };
        dto::Payment {
            operation_id: Some(op.id.clone()),
            bill_id: op.bill_id.clone(),
            state,
            reason_code,
            confirmations,
            next_action,
            retry_after_seconds: matches!(next_action, NextAction::Wait).then_some(30),
        }
    }

    fn refused(bill_id: &str, reason: Reason, next_action: NextAction) -> dto::Payment {
        dto::Payment {
            operation_id: None,
            bill_id: bill_id.to_string(),
            state: if matches!(next_action, NextAction::OwnerReview) {
                "waiting_for_owner"
            } else {
                "refused"
            },
            reason_code: Some(reason),
            confirmations: None,
            next_action,
            retry_after_seconds: None,
        }
    }

    pub fn agent_status(&self) -> dto::WalletStatus {
        let sync = self.sync_status();
        let behind = sync
            .map(|s| s.chain_tip.saturating_sub(s.fully_scanned))
            .unwrap_or(u32::MAX);
        let balance = self.wallet.balance().ok();
        let waiting = self
            .store
            .bills()
            .map(|bills| bills.iter().filter(|b| b.status == "needs_approval").count() as u32)
            .unwrap_or(0);
        let used = self.store.daily_used().unwrap_or(0);
        dto::WalletStatus {
            network: match self.net {
                Net::Testnet => "testnet",
                Net::Mainnet => "mainnet",
            },
            synced: sync.is_some() && behind <= MAX_BLOCKS_BEHIND,
            blocks_behind: if sync.is_some() { behind } else { 0 },
            unlocked: self.keys().is_some(),
            payments_paused: self.paused(),
            spendable: self.amount(balance.map(|b| b.spendable).unwrap_or(0)),
            pending: self.amount(balance.map(|b| b.pending).unwrap_or(0)),
            daily_limit_remaining: self.amount(self.daily_limit().saturating_sub(used)),
            bills_waiting_for_owner: waiting,
        }
    }

    pub fn agent_contacts(&self) -> Vec<dto::Contact> {
        self.store
            .contacts()
            .unwrap_or_default()
            .into_iter()
            .map(|c| dto::Contact {
                contact_id: c.id,
                alias: c.alias,
            })
            .collect()
    }

    pub fn agent_bills(&self, status: Option<&str>) -> Vec<dto::Bill> {
        self.store
            .bills()
            .unwrap_or_default()
            .iter()
            .map(|b| self.bill_dto(b))
            .filter(|b| status.is_none_or(|s| b.status == s))
            .collect()
    }

    pub fn agent_request_approval(&self, bill_id: &str) -> Result<dto::ApprovalRequested, dto::Error> {
        let bill = self
            .store
            .bill(bill_id)
            .ok()
            .flatten()
            .ok_or(err(Reason::NotFound, NextAction::None))?;
        if bill.status == "needs_approval" {
            if !self.store.approval_requested(bill_id).unwrap_or(false)
                && self.store.approval_requests_today().unwrap_or(0) >= MAX_APPROVAL_REQUESTS_PER_DAY
            {
                return Err(err(Reason::TooManyRequests, NextAction::RetryLater));
            }
            self.store
                .request_approval(bill_id)
                .map_err(|_| err(Reason::InternalError, NextAction::RetryLater))?;
            let _ = self.store.log("approval_requested", Some(bill_id), None);
        }
        Ok(dto::ApprovalRequested {
            bill_id: bill.id.clone(),
            state: self.bill_status(&bill),
            next_action: if bill.status == "needs_approval" {
                NextAction::OwnerReview
            } else {
                NextAction::None
            },
        })
    }

    /// Pays an approved bill. Repeating the call returns the same operation.
    pub fn agent_pay_bill(&self, bill_id: &str) -> dto::Payment {
        let Ok(Some(bill)) = self.store.bill(bill_id) else {
            return Self::refused(bill_id, Reason::NotFound, NextAction::None);
        };
        if let Ok(Some(op)) = self.store.operation_for_bill(bill_id) {
            return self.payment_dto(&op);
        }
        if self.paused() {
            return Self::refused(bill_id, Reason::PaymentsPaused, NextAction::OwnerReview);
        }
        let Some(keys) = self.keys() else {
            return Self::refused(bill_id, Reason::WalletLocked, NextAction::OwnerReview);
        };
        let approval = match self.store.approval_for_bill(bill_id) {
            Ok(Some(approval)) => approval,
            _ => {
                let reason = if bill.status == "rejected" {
                    Reason::NotApproved
                } else if self.store.approval_requested(bill_id).unwrap_or(false) {
                    Reason::ApprovalPending
                } else {
                    Reason::NotApproved
                };
                return Self::refused(bill_id, reason, NextAction::OwnerReview);
            }
        };
        match approval.status.as_str() {
            "active" => {}
            "revoked" => return Self::refused(bill_id, Reason::ApprovalRevoked, NextAction::OwnerReview),
            "used" => return Self::refused(bill_id, Reason::AlreadyPaid, NextAction::None),
            _ => return Self::refused(bill_id, Reason::NotApproved, NextAction::OwnerReview),
        }
        if approval.expires_at < now() {
            let _ = self.store.set_approval_status(&approval.id, "expired");
            return Self::refused(bill_id, Reason::ApprovalExpired, NextAction::OwnerReview);
        }
        let valid = policy::verify(&keys.approval_key, &approval.snapshot, &approval.mac)
            && serde_json::from_slice::<Snapshot>(&approval.snapshot)
                .map(|s| s.matches(self.net, &bill))
                .unwrap_or(false);
        if !valid {
            let _ = self.store.log("approval_mismatch", Some(bill_id), None);
            return Self::refused(bill_id, Reason::ApprovalMismatch, NextAction::OwnerReview);
        }
        match self.wallet.check_recipient(&bill.address) {
            Ok(()) => {}
            Err(RecipientError::WrongNetwork) => {
                return Self::refused(bill_id, Reason::WrongNetwork, NextAction::None);
            }
            Err(_) => return Self::refused(bill_id, Reason::UnsupportedRecipient, NextAction::None),
        }
        match self.sync_status() {
            Some(s) if s.chain_tip.saturating_sub(s.fully_scanned) <= MAX_BLOCKS_BEHIND => {}
            _ => {
                let mut refused = Self::refused(bill_id, Reason::NotSynced, NextAction::RetryLater);
                refused.retry_after_seconds = Some(30);
                return refused;
            }
        }
        let spendable = self.wallet.balance().map(|b| b.spendable).unwrap_or(0);
        if spendable < bill.amount.saturating_add(approval.fee_limit) {
            return Self::refused(bill_id, Reason::InsufficientFunds, NextAction::OwnerReview);
        }
        match self.store.reserve_payment(
            bill_id,
            &approval.id,
            bill.amount,
            approval.fee_limit,
            self.daily_limit(),
        ) {
            Ok(Ok(Reservation::New(op))) => {
                let _ = self.store.log("payment_reserved", Some(&op.id), None);
                self.notify.notify_one();
                self.payment_dto(&op)
            }
            Ok(Ok(Reservation::Existing(op))) => self.payment_dto(&op),
            Ok(Err(code)) => Self::refused(bill_id, Reason::from_code(code), NextAction::OwnerReview),
            Err(e) => {
                tracing::warn!("reserving payment: {e:#}");
                Self::refused(bill_id, Reason::InternalError, NextAction::RetryLater)
            }
        }
    }

    pub fn agent_payment(&self, operation_id: &str) -> Result<dto::Payment, dto::Error> {
        self.store
            .operation(operation_id)
            .ok()
            .flatten()
            .map(|op| self.payment_dto(&op))
            .ok_or(err(Reason::NotFound, NextAction::None))
    }

    fn request_dto(&self, req: &store::PaymentRequest) -> dto::Request {
        let tip = self.sync_status().map(|s| s.chain_tip).unwrap_or(0);
        let received = self.wallet.received_at(&req.address).unwrap_or_default();
        let detected: u64 = received.iter().map(|(v, _)| v).sum();
        let confs = |h: &Option<u32>| h.map(|h| tip.saturating_sub(h) + 1).unwrap_or(0);
        let confirmed: u64 = received
            .iter()
            .filter(|(_, h)| confs(h) >= DONE_CONFIRMATIONS)
            .map(|(v, _)| v)
            .sum();
        let confirmations = received.iter().map(|(_, h)| confs(h)).min().unwrap_or(0);
        let state = if confirmed >= req.amount {
            if confirmed > req.amount { "overpaid" } else { "paid" }
        } else if detected >= req.amount {
            "payment_detected"
        } else if detected > 0 {
            "partially_paid"
        } else {
            "not_paid"
        };
        dto::Request {
            request_id: req.id.clone(),
            label: req.label.clone(),
            amount: self.amount(req.amount),
            delivery: if req.delivery == "delivered_to_folder" {
                "delivered_to_folder"
            } else {
                "owner_will_send"
            },
            state,
            detected: self.amount(detected),
            confirmed: self.amount(confirmed),
            confirmations,
            next_action: match state {
                "paid" | "overpaid" => NextAction::None,
                _ => NextAction::Wait,
            },
        }
    }

    pub async fn agent_request_payment(
        &self,
        amount: &str,
        label: &str,
        contact_id: Option<&str>,
        idempotency_key: &str,
    ) -> Result<dto::Request, dto::Error> {
        let amount = parse_zec(amount).ok_or(err(Reason::InvalidAmount, NextAction::None))?;
        let label = label.trim();
        let key = idempotency_key.trim();
        if label.is_empty()
            || label.chars().count() > MAX_LABEL_LEN
            || label.chars().any(char::is_control)
            || dto::leaks(&json!(label))
            || key.is_empty()
            || key.len() > 100
        {
            return Err(err(Reason::InvalidAmount, NextAction::None));
        }
        if let Ok(Some(existing)) = self.store.request_by_key(key) {
            return Ok(self.request_dto(&existing));
        }
        if self.store.open_requests().unwrap_or(0) >= MAX_OPEN_REQUESTS_PER_DAY {
            return Err(err(Reason::TooManyRequests, NextAction::RetryLater));
        }
        let contact = match contact_id {
            Some(id) => Some(
                self.store
                    .contact(id)
                    .ok()
                    .flatten()
                    .ok_or(err(Reason::NotFound, NextAction::None))?,
            ),
            None => None,
        };
        let internal = |_| err(Reason::InternalError, NextAction::RetryLater);
        let address = {
            let _guard = self.wallet_lock.lock().await;
            self.wallet.new_receive_address().map_err(internal)?
        };
        let uri = invoice::build_request_uri(&address, amount, label).map_err(internal)?;
        let id = new_id("req");
        let delivery = match contact.as_ref().and_then(|c| c.delivery_dir.as_ref()) {
            Some(dir) if deliver(Path::new(dir), &id, &uri).is_ok() => "delivered_to_folder",
            _ => "owner_will_send",
        };
        let req = store::PaymentRequest {
            id,
            idempotency_key: key.to_string(),
            address,
            amount,
            label: label.to_string(),
            contact_id: contact.map(|c| c.id),
            uri,
            delivery: delivery.to_string(),
            created_at: now(),
        };
        self.store.insert_request(&req).map_err(internal)?;
        let _ = self.store.log("request_created", Some(&req.id), Some(delivery));
        Ok(self.request_dto(&req))
    }

    pub fn agent_check_request(&self, request_id: &str) -> Result<dto::Request, dto::Error> {
        self.store
            .request(request_id)
            .ok()
            .flatten()
            .map(|r| self.request_dto(&r))
            .ok_or(err(Reason::NotFound, NextAction::None))
    }

    pub fn agent_activity(&self, limit: u32) -> Vec<dto::Activity> {
        let limit = limit.clamp(1, MAX_ACTIVITY);
        self.store
            .events(limit)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| !e.kind.starts_with("owner_") && e.kind != "leak_blocked")
            .map(|e| {
                let (payee, amount) = match e.ref_id.as_deref() {
                    Some(id) if id.starts_with("bill_") => self
                        .store
                        .bill(id)
                        .ok()
                        .flatten()
                        .map(|b| (Some(self.alias(b.contact_id.as_deref())), Some(self.amount(b.amount))))
                        .unwrap_or((None, None)),
                    Some(id) if id.starts_with("op_") => self
                        .store
                        .operation(id)
                        .ok()
                        .flatten()
                        .and_then(|op| self.store.bill(&op.bill_id).ok().flatten())
                        .map(|b| (Some(self.alias(b.contact_id.as_deref())), Some(self.amount(b.amount))))
                        .unwrap_or((None, None)),
                    Some(id) if id.starts_with("req_") => self
                        .store
                        .request(id)
                        .ok()
                        .flatten()
                        .map(|r| (None, Some(self.amount(r.amount))))
                        .unwrap_or((None, None)),
                    _ => (None, None),
                };
                dto::Activity {
                    at: dto::rfc3339(e.at),
                    kind: e.kind,
                    payee,
                    amount,
                    ref_id: e.ref_id.filter(|id| {
                        ["bill_", "op_", "req_"].iter().any(|p| id.starts_with(p))
                    }),
                }
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct ReasonCode(#[serde(with = "reason_serde")] Reason);

mod reason_serde {
    use super::Reason;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Reason, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "over_fee_limit" => Reason::OverFeeLimit,
            "insufficient_funds" => Reason::InsufficientFunds,
            "approval_mismatch" => Reason::ApprovalMismatch,
            "approval_expired" => Reason::ApprovalExpired,
            "not_approved" => Reason::NotApproved,
            _ => Reason::InternalError,
        })
    }
}

fn err(reason_code: Reason, next_action: NextAction) -> dto::Error {
    dto::Error {
        reason_code,
        next_action,
    }
}

/// Writes a payment link into a contact's delivery folder atomically.
fn deliver(dir: &Path, id: &str, uri: &str) -> anyhow::Result<()> {
    let tmp = dir.join(format!(".{id}.tmp"));
    std::fs::write(&tmp, uri)?;
    std::fs::rename(&tmp, dir.join(format!("{id}.txt")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

type Shared = Arc<Daemon>;

fn router(daemon: Shared) -> Router {
    let agent = Router::new()
        .route("/status", get(agent_status))
        .route("/contacts", get(agent_contacts))
        .route("/bills", get(agent_bills))
        .route("/bills/{id}/request-approval", post(agent_request_approval))
        .route("/bills/{id}/pay", post(agent_pay))
        .route("/payments/{id}", get(agent_payment))
        .route("/requests", post(agent_create_request))
        .route("/requests/{id}", get(agent_get_request))
        .route("/activity", get(agent_activity))
        .layer(middleware::from_fn_with_state(daemon.clone(), require_agent));

    let owner = Router::new()
        .route("/session", get(owner_session))
        .route("/login", post(owner_login))
        .route("/logout", post(owner_logout))
        .route("/lock", post(owner_lock))
        .route("/overview", get(owner_overview))
        .route("/bills", get(owner_bills))
        .route("/bills/import", post(owner_import))
        .route("/bills/{id}/approve", post(owner_approve))
        .route("/bills/{id}/reject", post(owner_reject))
        .route("/approvals/{id}/revoke", post(owner_revoke))
        .route("/contacts", get(owner_contacts).post(owner_add_contact))
        .route("/requests", get(owner_requests))
        .route("/settings", post(owner_settings))
        .route("/events", get(owner_events))
        .layer(middleware::from_fn_with_state(daemon.clone(), require_owner));

    Router::new()
        .route("/", get(ui_index))
        .route("/app.js", get(ui_js))
        .route("/app.css", get(ui_css))
        .nest("/api/agent", agent)
        .nest("/api/owner", owner)
        .merge(playground::routes())
        .layer(middleware::from_fn_with_state(daemon.clone(), check_host))
        .with_state(daemon)
}

async fn check_host(State(d): State<Shared>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if !d.allowed_hosts.iter().any(|h| h == host) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        let origin_host = origin
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        if !d.allowed_hosts.iter().any(|h| h == origin_host) {
            return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
        }
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn require_agent(State(d): State<Shared>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| constant_eq(token.as_bytes(), d.agent_token.as_bytes()));
    if !ok {
        return (StatusCode::UNAUTHORIZED, Json(json!({"reason_code": "unauthorized"})))
            .into_response();
    }
    next.run(req).await
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Serialises a model-visible response, refusing it if anything looks like an
/// address, key or txid.
fn agent_json<T: serde::Serialize>(d: &Daemon, status: StatusCode, value: &T) -> Response {
    let value = match serde_json::to_value(value) {
        Ok(v) => v,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "serialisation").into_response(),
    };
    if dto::leaks(&value) {
        let _ = d.store.log("leak_blocked", None, None);
        tracing::error!("blocked an agent response that looked like it contained an address or txid");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"reason_code": "internal_error", "next_action": "retry_later"})),
        )
            .into_response();
    }
    (status, Json(value)).into_response()
}

fn agent_result<T: serde::Serialize>(d: &Daemon, result: Result<T, dto::Error>) -> Response {
    match result {
        Ok(v) => agent_json(d, StatusCode::OK, &v),
        Err(e) => {
            let status = if e.reason_code == Reason::NotFound {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            agent_json(d, status, &e)
        }
    }
}

async fn agent_status(State(d): State<Shared>) -> Response {
    let status = d.agent_status();
    agent_json(&d, StatusCode::OK, &status)
}

async fn agent_contacts(State(d): State<Shared>) -> Response {
    agent_json(&d, StatusCode::OK, &d.agent_contacts())
}

#[derive(Deserialize)]
struct BillsQuery {
    status: Option<String>,
}

async fn agent_bills(State(d): State<Shared>, Query(q): Query<BillsQuery>) -> Response {
    agent_json(&d, StatusCode::OK, &d.agent_bills(q.status.as_deref()))
}

async fn agent_request_approval(State(d): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    let result = d.agent_request_approval(&id);
    agent_result(&d, result)
}

async fn agent_pay(State(d): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    let payment = d.agent_pay_bill(&id);
    agent_json(&d, StatusCode::OK, &payment)
}

async fn agent_payment(State(d): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    let result = d.agent_payment(&id);
    agent_result(&d, result)
}

#[derive(Deserialize)]
struct CreateRequest {
    amount: String,
    label: String,
    contact_id: Option<String>,
    idempotency_key: String,
}

async fn agent_create_request(State(d): State<Shared>, Json(body): Json<CreateRequest>) -> Response {
    let result = d
        .agent_request_payment(
            &body.amount,
            &body.label,
            body.contact_id.as_deref(),
            &body.idempotency_key,
        )
        .await;
    agent_result(&d, result)
}

async fn agent_get_request(State(d): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    let result = d.agent_check_request(&id);
    agent_result(&d, result)
}

#[derive(Deserialize)]
struct ActivityQuery {
    limit: Option<u32>,
}

async fn agent_activity(State(d): State<Shared>, Query(q): Query<ActivityQuery>) -> Response {
    agent_json(&d, StatusCode::OK, &d.agent_activity(q.limit.unwrap_or(20)))
}

// ---- owner side ------------------------------------------------------------

fn session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().strip_prefix("zaw_session="))
        .map(str::to_string)
        .next()
}

fn valid_session(d: &Daemon, headers: &HeaderMap) -> Option<Session> {
    let id = session_id(headers)?;
    let mut sessions = d.sessions.lock().ok()?;
    sessions.retain(|_, s| s.expires_at > now());
    sessions.get(&id).cloned()
}

async fn require_owner(State(d): State<Shared>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path.ends_with("/login") || path.ends_with("/session") {
        return next.run(req).await;
    }
    let Some(session) = valid_session(&d, req.headers()) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "login required"}))).into_response();
    };
    if req.method() != axum::http::Method::GET {
        let csrf_ok = req
            .headers()
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| constant_eq(t.as_bytes(), session.csrf.as_bytes()));
        if !csrf_ok {
            return (StatusCode::FORBIDDEN, Json(json!({"error": "bad csrf token"}))).into_response();
        }
    }
    next.run(req).await
}

fn owner_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Password attempts slow down after a few failures. The counter is stored, so a
/// restart does not reset it.
fn login_delay(d: &Daemon) -> i64 {
    let until: i64 = d
        .store
        .get_kv("login_blocked_until")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    until - now()
}

fn record_password_result(d: &Daemon, ok: bool) {
    if ok {
        let _ = d.store.set_kv("login_failures", "0");
        let _ = d.store.set_kv("login_blocked_until", "0");
        return;
    }
    let failures: u32 = d
        .store
        .get_kv("login_failures")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        + 1;
    let _ = d.store.set_kv("login_failures", &failures.to_string());
    if failures >= 3 {
        let delay = 2i64.pow((failures - 3).min(10)).min(900);
        let _ = d
            .store
            .set_kv("login_blocked_until", &(now() + delay).to_string());
    }
    let _ = d.store.log("owner_password_failed", None, None);
}

async fn check_password(d: &Daemon, password: String) -> Result<Arc<UnlockedKeys>, Response> {
    let wait = login_delay(d);
    if wait > 0 {
        return Err(owner_error(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("too many wrong passwords, try again in {wait} s"),
        ));
    }
    match d.unlock(password).await {
        Ok(keys) => {
            record_password_result(d, true);
            Ok(keys)
        }
        Err(_) => {
            record_password_result(d, false);
            Err(owner_error(StatusCode::UNAUTHORIZED, "wrong password"))
        }
    }
}

#[derive(Deserialize)]
struct Password {
    password: String,
}

async fn owner_session(State(d): State<Shared>, headers: HeaderMap) -> Response {
    let session = valid_session(&d, &headers);
    Json(json!({
        "logged_in": session.is_some(),
        "csrf": session.map(|s| s.csrf),
        "unlocked": d.keys().is_some(),
        "network": d.net,
    }))
    .into_response()
}

async fn owner_login(State(d): State<Shared>, Json(body): Json<Password>) -> Response {
    let keys = match check_password(&d, body.password).await {
        Ok(keys) => keys,
        Err(res) => return res,
    };
    if let Ok(mut k) = d.keys.lock() {
        *k = Some(keys);
    }
    let id = random_token();
    let csrf = random_token();
    if let Ok(mut sessions) = d.sessions.lock() {
        sessions.insert(
            id.clone(),
            Session {
                csrf: csrf.clone(),
                expires_at: now() + SESSION_TTL_SECS,
            },
        );
    }
    let _ = d.store.log("owner_login", None, None);
    let secure = if d.secure_cookies { "; Secure" } else { "" };
    let cookie = format!(
        "zaw_session={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}{secure}"
    );
    (
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "csrf": csrf })),
    )
        .into_response()
}

async fn owner_logout(State(d): State<Shared>, headers: HeaderMap) -> Response {
    if let (Some(id), Ok(mut sessions)) = (session_id(&headers), d.sessions.lock()) {
        sessions.remove(&id);
    }
    (
        [(header::SET_COOKIE, "zaw_session=; Max-Age=0; Path=/".to_string())],
        Json(json!({})),
    )
        .into_response()
}

async fn owner_lock(State(d): State<Shared>) -> Response {
    if let Ok(mut k) = d.keys.lock() {
        *k = None;
    }
    let _ = d.store.log("owner_locked", None, None);
    Json(json!({ "unlocked": false })).into_response()
}

async fn owner_overview(State(d): State<Shared>) -> Response {
    let sync = d.sync_status();
    let balance = d.wallet.balance().ok();
    let bills = d.store.bills().unwrap_or_default();
    Json(json!({
        "network": d.net,
        "sync": sync,
        "balance": balance,
        "unlocked": d.keys().is_some(),
        "paused": d.paused(),
        "daily_limit": d.daily_limit(),
        "daily_used": d.store.daily_used().unwrap_or(0),
        "needs_approval": bills.iter().filter(|b| b.status == "needs_approval").count(),
        "approval_requested": bills.iter().filter(|b| d.store.approval_requested(&b.id).unwrap_or(false) && b.status == "needs_approval").count(),
        "agent_view": d.agent_status(),
    }))
    .into_response()
}

async fn owner_bills(State(d): State<Shared>) -> Response {
    let contacts = d.store.contacts().unwrap_or_default();
    let bills: Vec<_> = d
        .store
        .bills()
        .unwrap_or_default()
        .iter()
        .map(|b| {
            let contact = b
                .contact_id
                .as_ref()
                .and_then(|id| contacts.iter().find(|c| &c.id == id));
            let address_matches_contact = d
                .store
                .contact_by_address(&b.address)
                .ok()
                .flatten()
                .is_some();
            json!({
                "bill": b,
                "memo_text": b.memo.as_ref().map(|m| String::from_utf8_lossy(m).trim_end_matches('\0').to_string()),
                "contact": contact,
                "address_matches_contact": address_matches_contact,
                "approval": d.store.approval_for_bill(&b.id).ok().flatten(),
                "approval_requested": d.store.approval_requested(&b.id).unwrap_or(false),
                "operation": d.store.operation_for_bill(&b.id).ok().flatten(),
                "model_view": d.bill_dto(b),
                "model_payment_view": d.store.operation_for_bill(&b.id).ok().flatten().map(|op| d.payment_dto(&op)),
            })
        })
        .collect();
    Json(bills).into_response()
}

#[derive(Deserialize)]
struct ImportBody {
    uri: String,
}

async fn owner_import(State(d): State<Shared>, Json(body): Json<ImportBody>) -> Response {
    match d.import_bill(&body.uri, "owner") {
        Ok((id, new)) => Json(json!({ "bill_id": id, "new": new })).into_response(),
        Err(e) => owner_error(StatusCode::UNPROCESSABLE_ENTITY, e.describe()),
    }
}

#[derive(Deserialize)]
struct ApproveBody {
    password: String,
    label: String,
    contact_id: Option<String>,
    fee_limit_zec: Option<String>,
    expires_hours: Option<i64>,
}

async fn owner_approve(
    State(d): State<Shared>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ApproveBody>,
) -> Response {
    let Ok(Some(bill)) = d.store.bill(&id) else {
        return owner_error(StatusCode::NOT_FOUND, "no such bill");
    };
    if bill.status != "needs_approval" {
        return owner_error(StatusCode::CONFLICT, "this bill is not waiting for approval");
    }
    let label = body.label.trim().to_string();
    if label.is_empty() || label.chars().count() > MAX_LABEL_LEN || dto::leaks(&json!(label)) {
        return owner_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the label must be short text without addresses; the model will see it",
        );
    }
    let contact_id = body.contact_id.filter(|c| !c.is_empty()).or(bill.contact_id.clone());
    if let Some(cid) = &contact_id {
        if d.store.contact(cid).ok().flatten().is_none() {
            return owner_error(StatusCode::UNPROCESSABLE_ENTITY, "no such contact");
        }
    }
    let fee_limit = match body.fee_limit_zec.as_deref() {
        Some(s) if !s.trim().is_empty() => match parse_zec(s) {
            Some(v) => v,
            None => return owner_error(StatusCode::UNPROCESSABLE_ENTITY, "bad fee limit"),
        },
        _ => DEFAULT_FEE_LIMIT,
    };
    let hours = body.expires_hours.unwrap_or(DEFAULT_APPROVAL_HOURS).clamp(1, 24 * 90);
    let keys = match check_password(&d, body.password).await {
        Ok(keys) => keys,
        Err(res) => return res,
    };
    if let Ok(mut k) = d.keys.lock() {
        *k = Some(keys.clone());
    }
    let approval_id = new_id("ap");
    let snapshot = Snapshot::for_bill(
        &approval_id,
        d.net,
        &bill,
        fee_limit,
        now() + hours * 3600,
        contact_id.clone(),
        label.clone(),
    );
    let bytes = snapshot.to_bytes();
    let mac = policy::sign(&keys.approval_key, &bytes);
    if let Err(e) = d.store.insert_approval(
        &approval_id,
        &bill.id,
        &bytes,
        &mac,
        fee_limit,
        snapshot.expires_at,
        contact_id.as_deref(),
        &label,
    ) {
        tracing::warn!("storing approval: {e:#}");
        return owner_error(StatusCode::INTERNAL_SERVER_ERROR, "could not store the approval");
    }
    let _ = d.store.log("bill_approved", Some(&bill.id), None);
    Json(json!({ "approval_id": approval_id })).into_response()
}

async fn owner_reject(State(d): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    match d.store.bill(&id) {
        Ok(Some(bill)) if bill.status == "needs_approval" => {
            let _ = d.store.set_bill_status(&id, "rejected");
            let _ = d.store.log("bill_rejected", Some(&id), None);
            Json(json!({ "status": "rejected" })).into_response()
        }
        Ok(Some(_)) => owner_error(StatusCode::CONFLICT, "only bills waiting for approval can be rejected"),
        _ => owner_error(StatusCode::NOT_FOUND, "no such bill"),
    }
}

async fn owner_revoke(State(d): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    match d.store.revoke_approval(&id) {
        Ok(true) => {
            let _ = d.store.log("approval_revoked", Some(&id), None);
            Json(json!({ "status": "revoked" })).into_response()
        }
        Ok(false) => owner_error(
            StatusCode::CONFLICT,
            "this approval is no longer active: a payment has already started or it was used",
        ),
        Err(_) => owner_error(StatusCode::INTERNAL_SERVER_ERROR, "could not revoke"),
    }
}

async fn owner_contacts(State(d): State<Shared>) -> Response {
    Json(d.store.contacts().unwrap_or_default()).into_response()
}

#[derive(Deserialize)]
struct ContactBody {
    local_name: String,
    alias: Option<String>,
    delivery_dir: Option<String>,
    #[serde(default)]
    addresses: Vec<String>,
}

async fn owner_add_contact(State(d): State<Shared>, Json(body): Json<ContactBody>) -> Response {
    let local_name = body.local_name.trim().to_string();
    let alias = body
        .alias
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| local_name.clone());
    if local_name.is_empty() || alias.chars().count() > MAX_LABEL_LEN || dto::leaks(&json!(alias)) {
        return owner_error(StatusCode::UNPROCESSABLE_ENTITY, "bad name or alias");
    }
    for address in &body.addresses {
        if d.wallet.check_recipient(address).is_err() {
            return owner_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "each address must be a unified address with an Orchard receiver on this network",
            );
        }
    }
    let delivery_dir = body.delivery_dir.filter(|p| !p.trim().is_empty());
    match d
        .store
        .add_contact(&local_name, &alias, delivery_dir.as_deref(), &body.addresses)
    {
        Ok(id) => Json(json!({ "contact_id": id })).into_response(),
        Err(_) => owner_error(StatusCode::CONFLICT, "could not add the contact (address already used?)"),
    }
}

async fn owner_requests(State(d): State<Shared>) -> Response {
    let requests: Vec<_> = d
        .store
        .requests()
        .unwrap_or_default()
        .iter()
        .map(|r| json!({ "request": r, "model_view": d.request_dto(r) }))
        .collect();
    Json(requests).into_response()
}

#[derive(Deserialize)]
struct SettingsBody {
    daily_limit_zec: Option<String>,
    paused: Option<bool>,
}

async fn owner_settings(State(d): State<Shared>, Json(body): Json<SettingsBody>) -> Response {
    if let Some(limit) = body.daily_limit_zec {
        let value = if limit.trim() == "0" {
            Some(0)
        } else {
            parse_zec(&limit)
        };
        match value {
            Some(v) => {
                let _ = d.store.set_kv("daily_limit", &v.to_string());
                let _ = d.store.log("owner_daily_limit_changed", None, None);
            }
            None => return owner_error(StatusCode::UNPROCESSABLE_ENTITY, "bad daily limit"),
        }
    }
    if let Some(paused) = body.paused {
        let _ = d.store.set_kv("paused", if paused { "1" } else { "0" });
        let _ = d
            .store
            .log(if paused { "owner_paused" } else { "owner_resumed" }, None, None);
        d.notify.notify_one();
    }
    Json(json!({ "paused": d.paused(), "daily_limit": d.daily_limit() })).into_response()
}

async fn owner_events(State(d): State<Shared>) -> Response {
    Json(d.store.events(200).unwrap_or_default()).into_response()
}

// ---- static owner page -----------------------------------------------------

async fn ui_index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("ui/index.html"),
    )
}

async fn ui_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("ui/app.js"),
    )
}

async fn ui_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("ui/app.css"),
    )
}
