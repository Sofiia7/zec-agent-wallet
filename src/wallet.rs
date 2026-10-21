//! Wallet adapter over librustzcash: create, sync, receive and send.
//!
//! Funds are received through an Orchard receiver and land in the Ironwood pool
//! (ZIP 258: since NU6.3 no new value may enter the Orchard pool).

mod cache;

use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, anyhow, bail};
use bip0039::{Count, English, Mnemonic};
use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;
use secrecy::{ExposeSecret, SecretVec};
use serde::{Deserialize, Serialize};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use zcash_address::ZcashAddress;
use zcash_client_backend::data_api::wallet::{
    ConfirmationsPolicy, SpendingKeys, create_proposed_transactions,
    input_selection::{GreedyInputSelector, GreedyInputSelectorError, SpendPolicy},
    propose_transfer,
};
use zcash_client_backend::data_api::{AccountBirthday, WalletRead, WalletWrite};
use zcash_client_backend::fees::{DustOutputPolicy, StandardFeeRule, zip317::SingleOutputChangeStrategy};
use zcash_client_backend::proto::service::{
    self, compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_client_backend::wallet::OvkPolicy;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::wallet::{commitment_tree, init::init_wallet_db};
use zcash_client_sqlite::{AccountUuid, WalletDb};
use zcash_keys::keys::{UnifiedAddressRequest, UnifiedSpendingKey};
use zcash_primitives::transaction::TxId;
use zcash_primitives::transaction::fees::zip317;
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::ShieldedPool;
use zcash_protocol::consensus::Network;
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;
use zip321::{Payment, TransactionRequest};

use cache::MemBlockCache;

pub type Db = WalletDb<rusqlite::Connection, Network, SystemClock, UnwrapErr<SysRng>>;

/// Confirmations required before received funds are spendable. Fixed here, not taken
/// from library defaults, so that a dependency update cannot silently change them.
const TRUSTED_CONFIRMATIONS: u32 = 3;
const UNTRUSTED_CONFIRMATIONS: u32 = 10;

/// How far below the chain tip a new wallet's birthday is set, to survive small reorgs.
const BIRTHDAY_OFFSET: u32 = 100;

const SYNC_BATCH_SIZE: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Net {
    Testnet,
    Mainnet,
}

impl Net {
    pub fn params(self) -> Network {
        match self {
            Net::Testnet => Network::TestNetwork,
            Net::Mainnet => Network::MainNetwork,
        }
    }

    pub fn default_lightwalletd(self) -> &'static str {
        match self {
            Net::Testnet => "https://testnet.zec.rocks:443",
            Net::Mainnet => "https://zec.rocks:443",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub network: Net,
    pub lightwalletd: String,
    pub birthday: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Balance {
    pub spendable: u64,
    pub pending: u64,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SyncStatus {
    pub chain_tip: u32,
    pub fully_scanned: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum TxStatus {
    /// Known to the wallet but not yet seen in a block.
    Unmined,
    Mined { height: u32, confirmations: u32 },
    /// Not known to this wallet.
    Unknown,
}

pub struct Wallet {
    dir: PathBuf,
    config: Config,
}

fn confirmations_policy() -> ConfirmationsPolicy {
    ConfirmationsPolicy::new(
        NonZeroU32::new(TRUSTED_CONFIRMATIONS).expect("nonzero"),
        NonZeroU32::new(UNTRUSTED_CONFIRMATIONS).expect("nonzero"),
    )
    .expect("trusted confirmations do not exceed untrusted ones")
}

impl Wallet {
    fn config_path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    fn seed_path(dir: &Path) -> PathBuf {
        dir.join("seed.age")
    }

    fn db_path(dir: &Path) -> PathBuf {
        dir.join("wallet.sqlite")
    }

    pub fn exists(dir: &Path) -> bool {
        Self::config_path(dir).exists()
    }

    /// Creates a new wallet with a fresh 24-word mnemonic, encrypted with `password`.
    /// Returns the wallet and the mnemonic so the owner can back it up once.
    pub async fn create(
        dir: &Path,
        network: Net,
        lightwalletd: Option<String>,
        password: &str,
    ) -> anyhow::Result<(Self, Mnemonic<English>)> {
        if Self::exists(dir) {
            bail!("a wallet already exists in {}", dir.display());
        }
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lightwalletd = lightwalletd.unwrap_or_else(|| network.default_lightwalletd().into());
        let params = network.params();

        let mut client = connect(&lightwalletd).await?;
        let tip = latest_height(&mut client).await?;
        let birthday_height = tip.saturating_sub(BIRTHDAY_OFFSET);
        let treestate = client
            .get_tree_state(service::BlockId {
                height: u64::from(birthday_height - 1),
                ..Default::default()
            })
            .await
            .context("fetching tree state for the wallet birthday")?
            .into_inner();
        let birthday = AccountBirthday::from_treestate(treestate, None)
            .map_err(|e| anyhow!("invalid tree state: {e:?}"))?;

        let mnemonic = <Mnemonic<English>>::generate(Count::Words24);
        let seed = SecretVec::new(mnemonic.to_seed("").to_vec());

        let mut db = open_db(&Self::db_path(dir), params)?;
        init_wallet_db(&mut db, None).map_err(|e| anyhow!("initialising wallet db: {e}"))?;
        db.create_account("zaw", &seed, &birthday, None)
            .map_err(|e| anyhow!("creating account: {e}"))?;

        let encrypted = encrypt_seed(mnemonic.phrase(), password)?;
        fs::write(Self::seed_path(dir), encrypted).context("writing encrypted seed")?;

        let config = Config {
            network,
            lightwalletd,
            birthday: u32::from(birthday.height()),
        };
        fs::write(Self::config_path(dir), serde_json::to_vec_pretty(&config)?)
            .context("writing config")?;

        Ok((
            Wallet {
                dir: dir.to_path_buf(),
                config,
            },
            mnemonic,
        ))
    }

    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let config: Config = serde_json::from_slice(
            &fs::read(Self::config_path(dir))
                .with_context(|| format!("no wallet in {}", dir.display()))?,
        )
        .context("reading config")?;
        Ok(Wallet {
            dir: dir.to_path_buf(),
            config,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn params(&self) -> Network {
        self.config.network.params()
    }

    fn db(&self) -> anyhow::Result<Db> {
        open_db(&Self::db_path(&self.dir), self.params())
    }

    fn account(db: &Db) -> anyhow::Result<AccountUuid> {
        db.get_account_ids()
            .map_err(|e| anyhow!("reading accounts: {e}"))?
            .first()
            .copied()
            .ok_or_else(|| anyhow!("wallet has no account"))
    }

    /// Checks the password by decrypting the seed. Returns the spending key.
    pub fn unlock(&self, password: &str) -> anyhow::Result<UnifiedSpendingKey> {
        let phrase = decrypt_seed(&fs::read(Self::seed_path(&self.dir))?, password)?;
        let mnemonic = <Mnemonic<English>>::from_phrase(phrase.expose_secret().as_str())
            .map_err(|e| anyhow!("stored mnemonic is invalid: {e:?}"))?;
        let seed = SecretVec::new(mnemonic.to_seed("").to_vec());
        UnifiedSpendingKey::from_seed(&self.params(), seed.expose_secret(), zip32::AccountId::ZERO)
            .map_err(|e| anyhow!("deriving spending key: {e:?}"))
    }

    pub async fn sync(&self) -> anyhow::Result<SyncStatus> {
        let mut client = connect(&self.config.lightwalletd).await?;
        let mut db = self.db()?;
        let cache = MemBlockCache::default();
        zcash_client_backend::sync::run(&mut client, &self.params(), &cache, &mut db, SYNC_BATCH_SIZE)
            .await
            .map_err(|e| anyhow!("sync failed: {e}"))?;
        self.sync_status()
    }

    pub fn sync_status(&self) -> anyhow::Result<SyncStatus> {
        let db = self.db()?;
        let summary = db
            .get_wallet_summary(confirmations_policy())
            .map_err(|e| anyhow!("reading wallet summary: {e}"))?
            .ok_or_else(|| anyhow!("wallet has not synced yet"))?;
        Ok(SyncStatus {
            chain_tip: u32::from(summary.chain_tip_height()),
            fully_scanned: u32::from(summary.fully_scanned_height()),
        })
    }

    pub fn balance(&self) -> anyhow::Result<Balance> {
        let db = self.db()?;
        let account = Self::account(&db)?;
        let summary = db
            .get_wallet_summary(confirmations_policy())
            .map_err(|e| anyhow!("reading wallet summary: {e}"))?
            .ok_or_else(|| anyhow!("wallet has not synced yet"))?;
        let balance = summary
            .account_balances()
            .get(&account)
            .ok_or_else(|| anyhow!("no balance for account"))?;
        let spendable = balance.ironwood_balance().spendable_value();
        let total = balance.total();
        Ok(Balance {
            spendable: spendable.into_u64(),
            pending: total.into_u64().saturating_sub(spendable.into_u64()),
            total: total.into_u64(),
        })
    }

    /// Generates a fresh diversified address with only an Orchard receiver, for one
    /// payment request.
    pub fn new_receive_address(&self) -> anyhow::Result<String> {
        let mut db = self.db()?;
        let account = Self::account(&db)?;
        let (ua, _) = db
            .get_next_available_address(account, UnifiedAddressRequest::ORCHARD)
            .map_err(|e| anyhow!("generating address: {e}"))?
            .ok_or_else(|| anyhow!("account not found"))?;
        Ok(ua.encode(&self.params()))
    }

    /// Builds, signs and stores a transaction paying `amount` to `address`.
    /// Does not broadcast: callers persist the txid first, then call [`Self::broadcast`].
    pub fn build_payment(
        &self,
        usk: &UnifiedSpendingKey,
        address: &str,
        amount: Zatoshis,
        memo: Option<MemoBytes>,
    ) -> anyhow::Result<TxId> {
        let params = self.params();
        let recipient =
            ZcashAddress::from_str(address).map_err(|e| anyhow!("invalid address: {e}"))?;
        let payment = Payment::new(recipient, Some(amount), memo, None, None, vec![])
            .map_err(|e| anyhow!("invalid payment: {e:?}"))?;
        let request =
            TransactionRequest::new(vec![payment]).map_err(|e| anyhow!("invalid request: {e}"))?;

        let mut db = self.db()?;
        let account = Self::account(&db)?;
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Ironwood,
            DustOutputPolicy::default(),
        );
        let proposal = propose_transfer::<_, _, _, _, commitment_tree::Error>(
            &mut db,
            &params,
            account,
            &GreedyInputSelector::new(),
            &change_strategy,
            request,
            confirmations_policy(),
            &SpendPolicy::default(),
            None,
            None,
        )
        .map_err(|e| anyhow!("proposing transfer: {e}"))?;

        let prover = LocalTxProver::bundled();
        let txids = create_proposed_transactions::<_, _, GreedyInputSelectorError, _, zip317::FeeError, _>(
            &mut db,
            &params,
            &SystemClock,
            &mut UnwrapErr(SysRng),
            &prover,
            &prover,
            &SpendingKeys::from_unified_spending_key(usk.clone()),
            OvkPolicy::Sender,
            &proposal,
            None,
        )
        .map_err(|e| anyhow!("creating transaction: {e}"))?;
        if txids.len() > 1 {
            bail!("multi-transaction proposals are not supported");
        }
        Ok(*txids.first())
    }

    /// Sends a stored transaction to lightwalletd. Safe to repeat: the same bytes are
    /// sent every time.
    pub async fn broadcast(&self, txid: TxId) -> anyhow::Result<()> {
        let tx = self
            .db()?
            .get_transaction(txid)
            .map_err(|e| anyhow!("reading transaction: {e}"))?
            .ok_or_else(|| anyhow!("transaction not found in wallet"))?;
        let mut raw = service::RawTransaction::default();
        tx.write(&mut raw.data).context("serialising transaction")?;
        let mut client = connect(&self.config.lightwalletd).await?;
        let response = client
            .send_transaction(raw)
            .await
            .context("sending transaction")?
            .into_inner();
        if response.error_code != 0 {
            bail!(
                "lightwalletd rejected the transaction ({}): {}",
                response.error_code,
                response.error_message
            );
        }
        Ok(())
    }

    pub fn tx_status(&self, txid: TxId) -> anyhow::Result<TxStatus> {
        let db = self.db()?;
        if db
            .get_transaction(txid)
            .map_err(|e| anyhow!("reading transaction: {e}"))?
            .is_none()
        {
            return Ok(TxStatus::Unknown);
        }
        let mined = db
            .get_tx_height(txid)
            .map_err(|e| anyhow!("reading transaction height: {e}"))?;
        let tip = db
            .chain_height()
            .map_err(|e| anyhow!("reading chain height: {e}"))?;
        Ok(match (mined, tip) {
            (Some(height), Some(tip)) => TxStatus::Mined {
                height: u32::from(height),
                confirmations: u32::from(tip).saturating_sub(u32::from(height)) + 1,
            },
            (Some(height), None) => TxStatus::Mined {
                height: u32::from(height),
                confirmations: 1,
            },
            (None, _) => TxStatus::Unmined,
        })
    }
}

pub fn parse_txid(hex_str: &str) -> anyhow::Result<TxId> {
    let mut bytes = hex::decode(hex_str.trim()).context("txid is not hex")?;
    bytes.reverse();
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("txid must be 32 bytes"))?;
    Ok(TxId::from_bytes(bytes))
}

fn open_db(path: &Path, params: Network) -> anyhow::Result<Db> {
    WalletDb::for_path(path, params, SystemClock, UnwrapErr(SysRng))
        .with_context(|| format!("opening {}", path.display()))
}

async fn connect(url: &str) -> anyhow::Result<CompactTxStreamerClient<Channel>> {
    let mut endpoint =
        Endpoint::from_shared(url.to_string()).with_context(|| format!("bad server url {url}"))?;
    if url.starts_with("https://") {
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().with_webpki_roots())
            .context("configuring TLS")?;
    }
    let channel = endpoint
        .connect()
        .await
        .with_context(|| format!("connecting to {url}"))?;
    Ok(CompactTxStreamerClient::new(channel))
}

async fn latest_height(client: &mut CompactTxStreamerClient<Channel>) -> anyhow::Result<u32> {
    let block = client
        .get_latest_block(service::ChainSpec::default())
        .await
        .context("fetching chain tip")?
        .into_inner();
    u32::try_from(block.height).context("chain tip height out of range")
}

fn encrypt_seed(phrase: &str, password: &str) -> anyhow::Result<Vec<u8>> {
    let recipient = age::scrypt::Recipient::new(age::secrecy::SecretString::from(password.to_owned()));
    age::encrypt(&recipient, phrase.as_bytes()).map_err(|e| anyhow!("encrypting seed: {e}"))
}

fn decrypt_seed(ciphertext: &[u8], password: &str) -> anyhow::Result<secrecy::SecretString> {
    let identity = age::scrypt::Identity::new(age::secrecy::SecretString::from(password.to_owned()));
    let plaintext = age::decrypt(&identity, ciphertext).map_err(|_| anyhow!("wrong password"))?;
    let phrase = String::from_utf8(plaintext).context("stored seed is not UTF-8")?;
    Ok(secrecy::SecretString::new(phrase))
}
