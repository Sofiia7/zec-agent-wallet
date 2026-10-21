# zec-agent-wallet

A local Zcash wallet that lets an AI agent pay owner-approved bills in shielded ZEC over the Model Context Protocol (MCP), without handing the model your keys, addresses or the messages your counterparties write. Working name.

**Status:** early development for ZECATHON (October 2026), tested on testnet only. Not ready for real funds.

## What works today (testnet)

- `zaw daemon`: light-client wallet (no node needed) that syncs through lightwalletd, receives on Orchard-only unified addresses (funds land in the Ironwood pool) and sends shielded payments.
- Owner approval of a specific bill: recipient, amount, memo, fee limit and expiry, confirmed with the wallet password. The agent can then pay that bill once, exactly as approved. Unapproved, changed or expired bills are refused with a fixed reason code.
- One payment per bill: repeated or concurrent `pay_bill` calls return the same operation. States are stored before each step; an interrupted build is never retried automatically.
- Payment requests (`request_payment`) on a fresh address per request, delivered to a folder or handed to the owner, and `check_request` that matches incoming funds by address and confirmations.
- `zaw mcp`: an MCP server over stdio with 9 tools. It holds no keys and returns only aliases, owner-set labels, amounts and statuses. Every response is checked for anything that looks like an address, key or transaction id before it leaves the process.
- A local owner window: approvals, bills with the sender's text shown separately, payment requests, a daily limit, a stop button for agent payments, and next to each item the exact response the model receives.
- `scripts/leak_check.py`: feeds canary strings and an address through a bill's memo, label and message and checks every agent API and MCP output, including stderr.

End to end on testnet: one agent issued a bill, the other side's owner approved it, its agent paid it, and the first agent saw it paid after 3 confirmations.

## What it does not promise

- The aliases, labels and amounts the model does see reach the model provider. The agent may also learn things from its other tools (email, browser).
- An agent that can run shell commands or read files as the same OS user (for example a coding agent) can read the wallet's local data. If your agent can run commands, run the wallet daemon as a separate OS user.
- A light client trusts its lightwalletd server for chain data, and the server sees your IP address.

See [docs/design.md](docs/design.md) for the approval model, failure handling and threat model.

## Build

Requires Rust 1.98.1 (pinned in `rust-toolchain.toml`). No `protoc` needed.

```
cargo build --release --locked
cargo test --locked
```

## Try it on testnet

```
# create a wallet (the password encrypts the seed)
ZAW_PASSWORD=... zaw dev init
zaw dev address            # get testnet coins (TAZ) from a faucet to this address

# run the daemon, then open http://127.0.0.1:7341 and unlock with the password
zaw daemon
```

Connect an MCP client, for example Claude Code:

```
claude mcp add zaw -- zaw mcp
```

The MCP server reads its access token from the wallet folder; the daemon must be running.

## License

MIT, see [LICENSE](LICENSE).
