mod daemon;
mod dto;
mod invoice;
mod mcp;
mod policy;
mod store;
mod wallet;

use std::path::PathBuf;

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use zcash_protocol::value::Zatoshis;

use wallet::{Net, Wallet};

#[derive(Parser)]
#[command(name = "zaw", version, about = "A shielded Zcash wallet for AI agents")]
struct Cli {
    /// Wallet data directory. Defaults to the OS data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[arg(long, global = true, value_enum, default_value_t = Net::Testnet)]
    network: Net,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the wallet daemon: sync, payments, the owner window and the agent API.
    Daemon {
        #[arg(long, default_value = "127.0.0.1:7341")]
        listen: String,
        /// Extra Host header to accept, for a daemon behind a reverse proxy.
        #[arg(long)]
        public_host: Option<String>,
        /// Unlock at start with the password in ZAW_PASSWORD (test wallets only).
        #[arg(long)]
        unlock_from_env: bool,
    },
    /// MCP server over stdio for an AI client. Holds no keys; talks to the daemon.
    Mcp {
        #[arg(long, default_value = "http://127.0.0.1:7341")]
        daemon_url: String,
    },
    /// Low-level wallet commands for development and diagnostics.
    #[command(subcommand)]
    Dev(Dev),
}

#[derive(Subcommand)]
enum Dev {
    /// Create a new wallet. Reads the password from ZAW_PASSWORD.
    Init {
        #[arg(long)]
        lightwalletd: Option<String>,
        /// Print the mnemonic once (only for test wallets).
        #[arg(long)]
        show_mnemonic: bool,
    },
    /// Scan the chain up to the current tip.
    Sync,
    /// Print a fresh receive address (Orchard receiver only).
    Address,
    /// Print the balance in zatoshis.
    Balance,
    /// Pay an address. Reads the password from ZAW_PASSWORD.
    Send {
        address: String,
        zatoshis: u64,
    },
    /// Show the status of a transaction.
    Status { txid: String },
}

fn default_data_dir(network: Net) -> anyhow::Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| anyhow!("no OS data directory"))?;
    Ok(base.join("zaw").join(match network {
        Net::Testnet => "testnet",
        Net::Mainnet => "mainnet",
    }))
}

fn password_from_env() -> anyhow::Result<String> {
    std::env::var("ZAW_PASSWORD").context("set ZAW_PASSWORD")
}

/// Proof construction recurses deeply; the 1 MiB default main-thread stack on
/// Windows overflows in debug builds. All work runs on runtime threads with this
/// stack size instead.
const THREAD_STACK_SIZE: usize = 64 * 1024 * 1024;

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK_SIZE)
        .build()?;
    runtime.block_on(async { tokio::spawn(real_main()).await? })
}

async fn real_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("installing the TLS crypto provider"))?;

    let cli = Cli::parse();
    let dir = match cli.data_dir {
        Some(dir) => dir,
        None => default_data_dir(cli.network)?,
    };

    match cli.command {
        Command::Daemon {
            listen,
            public_host,
            unlock_from_env,
        } => {
            let unlock_password = if unlock_from_env {
                Some(password_from_env()?)
            } else {
                None
            };
            daemon::run(
                &dir,
                daemon::Options {
                    listen,
                    public_host,
                    unlock_password,
                },
            )
            .await
        }
        Command::Mcp { daemon_url } => {
            let token = std::fs::read_to_string(dir.join("agent-token"))
                .context("no agent token: start the daemon once first")?
                .trim()
                .to_string();
            mcp::run(mcp::Options { daemon_url, token }).await
        }
        Command::Dev(dev) => run_dev(dev, &dir, cli.network).await,
    }
}

async fn run_dev(dev: Dev, dir: &std::path::Path, network: Net) -> anyhow::Result<()> {
    match dev {
        Dev::Init {
            lightwalletd,
            show_mnemonic,
        } => {
            let password = password_from_env()?;
            let (wallet, mnemonic) = Wallet::create(dir, network, lightwalletd, &password).await?;
            println!(
                "created {:?} wallet in {}, birthday {}",
                wallet.config().network,
                dir.display(),
                wallet.config().birthday
            );
            if show_mnemonic {
                println!("{}", mnemonic.phrase());
            }
        }
        Dev::Sync => {
            let status = Wallet::open(dir)?.sync().await?;
            println!("synced: tip {}, scanned {}", status.chain_tip, status.fully_scanned);
        }
        Dev::Address => println!("{}", Wallet::open(dir)?.new_receive_address()?),
        Dev::Balance => {
            let balance = Wallet::open(dir)?.balance()?;
            println!(
                "spendable {} pending {} total {}",
                balance.spendable, balance.pending, balance.total
            );
        }
        Dev::Send { address, zatoshis } => {
            let wallet = Wallet::open(dir)?;
            let keys = wallet.unlock(&password_from_env()?)?;
            wallet.sync().await?;
            let amount = Zatoshis::from_u64(zatoshis).map_err(|_| anyhow!("invalid amount"))?;
            let (txid, fee) = wallet
                .build_payment(&keys.usk, &address, amount, None, 1_000_000)?
                .map_err(|refusal| anyhow!("refused: {refusal:?}"))?;
            println!("fee {fee}");
            println!("built {txid}");
            wallet.broadcast(txid).await?;
            println!("broadcast {txid}");
        }
        Dev::Status { txid } => {
            let wallet = Wallet::open(dir)?;
            wallet.sync().await?;
            println!("{:?}", wallet.tx_status(wallet::parse_txid(&txid)?)?);
        }
    }
    Ok(())
}
