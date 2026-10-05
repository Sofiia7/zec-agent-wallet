# zec-agent-wallet

A shielded Zcash wallet for AI agents, exposed over the Model Context Protocol (MCP). Working name.

**Status:** early development for ZECATHON (October 2026). Not ready for real funds.

## Idea

An AI agent should be able to pay and get paid in shielded ZEC without telling the model provider who pays whom, and without being talked into paying more than its owner agreed to.

- The model works with contact names, amounts and statuses. It never sees addresses, keys, memo text or transaction ids.
- The wallet pays on its own only what the owner approved in advance: the same amount, to the same saved address, within limits.
- Everything else waits for the owner in a local approval window protected by a PIN.
- A built-in light client: no Zcash node of your own is needed.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
