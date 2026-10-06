# zec-agent-wallet

A local Zcash wallet that lets an AI agent pay owner-approved bills in shielded ZEC over the Model Context Protocol (MCP), without handing the model your keys, addresses or the messages your counterparties write. Working name.

**Status:** early development for ZECATHON (October 2026). Nothing below works yet: this describes planned behavior. Not ready for real funds.

## Planned behavior

- The owner approves a specific bill: recipient, amount, fee cap and deadline. The agent can pay that bill later, once, exactly as approved. Everything else waits for the owner.
- The wallet never sends the model keys, addresses, transaction ids or text written by counterparties (memo, label, message). The model sees contact aliases, bill labels, amounts in ZEC with approximate USD, and statuses.
- Retries, timeouts and crashes never turn into a second payment.
- Orchard-only shielded payments in the first version.
- A built-in light client: no Zcash node of your own is needed.

## What it does not promise

- The aliases, labels and amounts the model does see reach the model provider. The agent may also learn things from its other tools (email, browser).
- An agent that can run shell commands or read files as the same OS user (for example a coding agent) can read the wallet's local data. If your agent can run commands, run the wallet daemon as a separate OS user.
- A light client trusts its lightwalletd server for chain data, and the server sees your IP address.

## License

MIT, see [LICENSE](LICENSE).
