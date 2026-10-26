# zec-agent-wallet

A local Zcash wallet that lets an AI agent pay owner-approved bills in shielded ZEC over the Model Context Protocol (MCP), without handing the model your keys, addresses or the messages your counterparties write. Working name.

**Status:** early development for ZECATHON (October 2026), tested on testnet and with one small mainnet payment. Not ready for real funds.

## What works today

- `zaw daemon`: light-client wallet for testnet and mainnet (no node needed) that syncs through lightwalletd, receives on Orchard-only unified addresses (funds land in the Ironwood pool) and sends shielded payments.
- Owner approval of a specific bill: recipient, amount, memo, fee limit and expiry, confirmed with the wallet password. The agent can then pay that bill once, exactly as approved. Unapproved, changed or expired bills are refused with a fixed reason code.
- One payment per bill: repeated or concurrent `pay_bill` calls return the same operation. States are stored before each step; an interrupted build is never retried automatically.
- Payment requests (`request_payment`) on a fresh address per request, delivered to a folder or handed to the owner, and `check_request` that matches incoming funds by address and confirmations.
- `zaw mcp`: an MCP server over stdio with 9 tools. It holds no keys and returns only aliases, owner-set labels, amounts and statuses. Every response is checked for anything that looks like an address, key or transaction id before it leaves the process.
- A local owner window: bills with their approval state (needs you, approved, paid), the sender's text and the address shown only to the owner, payment requests, a daily limit, a stop button for agent payments, a plain-language activity log, and for each bill the exact response the model receives.
- `scripts/leak_check.py`: feeds canary strings and an address through a bill's memo, label and message and checks every agent API and MCP output, including stderr.

End to end on testnet: one agent issued a bill, the other side's owner approved it, its agent paid it, and the first agent saw it paid after 3 confirmations.

Once on mainnet (26 Oct 2026): the same flow between two wallets, with the payer's agent running on the Claude API ([examples/claude_agent.py](examples/claude_agent.py)). The 0.001 ZEC bill was paid with a 0.0001 ZEC fee and seen as paid about 3 minutes after `pay_bill`. Claude's desktop app declines to move real funds on its own rules, so on mainnet use an agent you run on the API.

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

## Try it

The quickest way is the hosted testnet playground, https://zaw.89-124-77-59.sslip.io/playground (the owner password is in the submission): two wallets run side by side, you press the buttons an agent would call and see the exact JSON the model receives. Approvals happen in the owner window.

To run it yourself on testnet:

```
zaw init                   # choose a password, write down the 24 words
zaw dev address            # a receive address; get test coins (TAZ) from a testnet faucet
zaw daemon                 # then open http://127.0.0.1:7341 and unlock with the password
```

On mainnet, add `--network mainnet` to each command (wallet data then lives in a separate `mainnet` folder) and start with a small amount.

Connect an MCP client. Claude Code:

```
claude mcp add zaw -- zaw mcp
```

Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "zaw": { "command": "C:\\path\\to\\zaw.exe", "args": ["mcp"] }
  }
}
```

Your own agent on the Claude API: [examples/claude_agent.py](examples/claude_agent.py) starts `zaw mcp`, gives Claude its tools and pays the bills the owner approved.

```
pip install "anthropic[mcp]"
python examples/claude_agent.py --zaw path/to/zaw "Pay the approved bill Logo #17"
```

The MCP server reads its access token from the wallet folder; the daemon must be running. Wallet data lives in the OS data folder (`%APPDATA%\zaw\testnet` on Windows, `~/.local/share/zaw/testnet` on Linux); `--data-dir` overrides it.

## License

MIT, see [LICENSE](LICENSE).
