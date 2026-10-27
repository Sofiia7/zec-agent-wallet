# Security

zec-agent-wallet is early software tested on testnet. Do not use it with funds you cannot lose.

## Reporting a vulnerability

Please report privately through GitHub: Security → Report a vulnerability on this repository. Do not open a public issue for anything that could move funds or expose wallet data.

## What the wallet trusts

| Component | Trusted with | Notes |
|---|---|---|
| Daemon (`zaw daemon`) | Seed (encrypted at rest with the owner's password, age/scrypt), spending key and approval key in memory while unlocked | Locking removes both keys from memory |
| Owner window | Approvals, settings | Password login, HttpOnly SameSite=Strict session cookie, CSRF token, Host and Origin checks, strict CSP, no framing |
| MCP server (`zaw mcp`) | Nothing secret: an agent API token | Cannot approve, unlock, change limits or recipients; the owner API rejects its token |
| lightwalletd server | Chain data | Cannot spend; sees IP and queries; a malicious server could show an incoming payment that is not on chain |
| ZEC/USD price service (Coinbase, Kraken as fallback) | A display price, only while the owner has USD amounts on (mainnet) | Fixed HTTPS URLs, nothing taken from input, so no SSRF; the reply must be a plain decimal within sane bounds; a wrong price changes only displayed dollars, never an amount paid; sees the IP address every 10 minutes |

The full threat model, including what is out of scope (an agent with shell access as the same OS user, computer use, root), is in [docs/design.md](docs/design.md#threat-model).

## Checks and what the code does

| Area | Check | Result |
|---|---|---|
| Secrets in git | gitleaks over the whole history | No findings. Wallet files, seeds and `.env` files are ignored; runtime data lives outside the checkout |
| Leaks to the model | `scripts/leak_check.py`: canary strings and an address in memo, label and message, then every agent API call and MCP tool, stdout and stderr | No canary, address, key or txid in any output; the owner API refuses the agent token |
| Double payment | Unit tests: repeated and 8 concurrent `pay_bill` reservations | One operation; a unique constraint on the bill and an immediate SQLite transaction |
| Approval tampering | Unit tests: any change to the snapshot or a different key | MAC fails; the daemon re-checks it right before signing |
| Login brute force | Growing delay after 3 wrong passwords, stored across restarts | Capped at 30 s on the public playground so a visitor cannot lock the owner out |
| SQL injection | Code review | All queries use bound parameters; the only formatted lists are compile-time constants |
| Untrusted input | Code review | Payment links: single recipient, strict ZIP 321 parsing, 2 KB limit. Inbox files: regular `.txt` only, no symlinks, 4 KB limit, partially written files skipped |
| Error detail | Code review | The model gets fixed reason codes only; library errors are logged locally, never returned to the agent |
| Dependencies | cargo-audit in CI over 482 crates (26 Oct 2026) | `ctutils` (RUSTSEC-2026-0385) updated to 0.4.6. One advisory ignored in `.cargo/audit.toml`: `crypto-bigint` 0.5.5 (RUSTSEC-2026-0387, timing variability) comes only through `age` -> `p256`, which `age` uses for p256tag recipients; the wallet encrypts the seed with the scrypt passphrase recipient only, so that code never runs. No `age` release with `crypto-bigint` 0.7 exists yet. One unmaintained-crate warning: `basic-toml` (RUSTSEC-2026-0374), used only at build time by the localisation macros of `age`; no untrusted TOML is parsed at run time |

## Known limits

- An agent with shell or file access as the same OS user can read the wallet database (addresses, memos) and tokens. Run the daemon as a separate OS user if your agent can run commands.
- The HMAC on approvals stops database edits from creating approvals, but not a process that can read the daemon's memory.
- The public playground shares one testnet wallet between all visitors and holds only test coins.
