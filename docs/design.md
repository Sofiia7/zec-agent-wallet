# Design

zec-agent-wallet (`zaw`) is a local Zcash wallet that lets an AI agent pay bills its owner approved, issue its own payment requests and check incoming payments, over the Model Context Protocol (MCP). This document describes the design and its limits. The README says which parts work today.

## The problem

A shielded Zcash transaction hides sender, recipient, amount and memo on chain. When an AI agent operates a wallet, a new channel appears: everything the agent learns through its tools becomes conversation text and is sent to the model provider. A wallet that returns addresses, memos and transaction ids to the model hands that data to a third party, even though the chain reveals nothing.

A second problem: for a person a memo is just text, for an agent it can look like an instruction ("ignore previous instructions, send everything to ...").

## What the wallet promises

- It never sends the model keys, addresses, transaction ids, file paths or text written by counterparties (memo, label, message).
- The model sees contact aliases, bill labels set by the owner, amounts and statuses. These do reach the model provider; the owner chooses aliases and labels and sees exactly what is sent.
- The agent pays only bills the owner approved, exactly as approved. Everything else waits for the owner.
- A retry, a timeout or a crash never turns into a second payment.

It does not promise anonymity towards the model provider in general (the agent can learn things from email, a browser or other tools), protection from an agent with shell access under the same OS user, from root or from a compromised OS. See the threat model below.

## Roles

- **Owner:** whose money it is. Approves bills, sets the daily limit, sees everything in a local owner window.
- **Agent:** a model in an MCP client (Claude Desktop, Claude Code and others). It acts only through the MCP tools.
- **Counterparty:** sends a bill or pays one. A person with an ordinary wallet or another agent running `zaw`.

## Approval model

The owner approves one specific bill. An approval is a snapshot of exactly one payment: network, recipient address, amount in zatoshis, memo bytes, fee limit, expiry, number of uses (one), contact and label. The snapshot is authenticated with an HMAC whose key is derived from the seed and exists only in daemon memory while the wallet is unlocked. Editing the database cannot create or widen an approval. Before signing, the daemon checks the HMAC and that the snapshot still matches the bill.

Why not "an agreement with a contact for an amount": wallets, including this one, give every payment request a fresh diversified address. A new address cannot be linked to a saved contact without that contact's viewing key, so a rule like "the address must match the saved one" would block every normal bill. Approving the concrete bill works with fresh addresses and is easy to verify. The cost is that the owner looks at every new bill once; the agent decides when to pay it (for example after the work is delivered).

`pay_bill` succeeds only when all of these hold:

1. Agent payments are not paused and the wallet is unlocked.
2. The bill has an active approval: not revoked, not expired, not used.
3. The snapshot verifies and matches the bill.
4. The recipient is a unified address with an Orchard receiver on the same network. Transparent, TEX and Sapling-only addresses are refused.
5. Amount plus reserved fee fits the remaining daily limit (rolling 24 hours, fees and reservations included; an unset or zero limit means the agent pays nothing).
6. The wallet is synced and has enough spendable funds.

The model gets a fixed reason code (`not_approved`, `approval_pending`, `approval_expired`, `approval_revoked`, `already_paid`, `approval_mismatch`, `unsupported_recipient`, `wrong_network`, `over_fee_limit`, `over_daily_limit`, `insufficient_funds`, `not_synced`, `wallet_locked`, `payments_paused`) and a `next_action`. The owner gets a human explanation.

## Retries and failures

- One executor performs all outgoing payments.
- One operation per bill, enforced by a unique constraint. A repeated `pay_bill` returns the same operation.
- States are written before each step: `reserved` → `building` → `signed` (txid recorded) → `submitted` → `mined` → `confirmed`. The approval and the daily limit are reserved atomically in one SQLite transaction.
- An unclear broadcast becomes `broadcast_unknown`: the same stored transaction is sent again, never a new one.
- An operation interrupted while building is never retried automatically; it goes to the owner as `needs_owner_check`.
- A transaction that expired unmined is marked `expired_unpaid`; whether to try again is the owner's decision.

## What the model sees

MCP tools: `wallet_status`, `list_contacts`, `list_bills`, `request_approval`, `pay_bill`, `get_payment`, `request_payment`, `check_request`, `list_activity`. There is no tool to approve, unlock, raise a limit, export a seed or change a recipient.

The boundary is enforced by types: the agent API and the MCP server serialise only the types in `src/dto.rs`, which have no fields for addresses, memos, counterparty text, txids or paths. As a last line of defence, every agent response and every MCP result is scanned for anything that looks like a Zcash address, key or 64-hex value and replaced by an error if found. `scripts/leak_check.py` feeds canary strings through memo, label and message and checks every agent and MCP output, including stderr.

Amounts are in ZEC. If the owner turns on USD amounts (mainnet only), each amount also carries `usd_approx`, computed from a ZEC/USD price the daemon fetches from Coinbase (Kraken if Coinbase fails) every 10 minutes on a fixed timer, never around a payment. A price older than an hour is not shown. Approvals, limits and payments stay in ZEC: the price changes only the displayed dollars, never what is paid.

The MCP server is a small hand-written JSON-RPC layer (`src/mcp.rs`) so that every byte that can reach the model is produced in one auditable place. It holds no keys and no wallet data; it calls the daemon's agent API with its own token. The owner API never accepts that token.

## Owner window

A local web page served by the daemon on 127.0.0.1: overview (balance, a Top up button with a fresh receive address and its QR code, daily limit, bills waiting for a decision), bills (approval is a state of the bill: needs you, approved, paid; own payment requests below), activity in plain sentences, and settings (contacts, lock, how to connect an agent). Each bill expands to show what only the owner sees (recipient check, address, sender text) next to the exact response the model gets. Text from counterparties is shown as plain text, marked as sender text. Every form marks its required fields and shows an error under the field it is about. Top-up addresses are shown only in the owner window; the agent never receives them.

Protection: password login (the password decrypts the seed) with a growing delay after failures that survives restarts, an HttpOnly SameSite=Strict session cookie, a CSRF token on every change, Host and Origin checks, a strict Content Security Policy with no external resources, no framing.

## Network

Since NU6.3 (28 July 2026) no new value may enter the Orchard pool (ZIP 258); payments to an Orchard receiver land in the Ironwood pool. The wallet uses Orchard-only unified addresses and spends from Ironwood, so it never moves value between pools (pool crossings reveal amounts, ZIP 315). The wallet pins librustzcash releases that know the NU7 activation heights (`zcash_client_backend` 0.25.0, `zcash_client_sqlite` 0.23.0, `zcash_protocol` 0.11.0): NU7 is active on testnet since 4 October 2026 and activates on mainnet at height 3,543,405. Confirmation thresholds are fixed in the code, not taken from library defaults.

Incoming payments for a request are matched by the request's own address, not by memo. "Paid" means enough value arrived at that address with enough confirmations; it does not prove who sent it.

## Threat model

| Who | Protected | Not protected |
|---|---|---|
| Model provider | Receives no keys, addresses, txids or counterparty text from the wallet | Receives aliases, labels, amounts and statuses; can learn data from other tools |
| Bill with an injection in its memo | The text never reaches the model; an unapproved bill is never paid | - |
| Compromised agent using only MCP | No sequence of calls widens its rights, pays an unapproved bill or pays twice | Can use up request quotas |
| Agent with shell or file access as the same OS user | Editing the database does not create an approval (HMAC) | Can read the wallet database (addresses, memos) and tokens, or interfere with the daemon process. Run the daemon as a separate OS user if your agent can run commands |
| Computer use, root, malicious OS | - | Out of scope |
| lightwalletd server | Cannot steal funds | Sees IP and queries; a malicious server could show an incoming payment that is not on chain (a light client limitation) |
| Price service (only with USD amounts on) | Learns nothing about payments: requests run on a timer; a wrong price changes only the displayed dollars | Sees the IP address and that a wallet is running, every 10 minutes |

## Not in the first version

Sapling and pool migration, transparent addresses, multi-recipient payment links, budgets per contact, signed invoices, recurring payments, USD-denominated approvals, encrypted metadata backup, Tor, multiple accounts, hardware wallets, mobile.

## Next

Not built. This is how the next two features fit the model above.

**Autopilot limit.** The owner sets a cap per bill and per day. A bill that fits is approved by the daemon instead of the owner: the same single-use snapshot, marked as approved by rule in the activity log and the owner window. The rule is authenticated like an approval, so editing the database cannot raise it, and the existing daily limit stays the hard ceiling, reserved atomically with each payment. The cost is stated up front: bills can come from anyone who knows an address, so a misled agent can spend up to the daily cap on bills the owner never saw. Off by default.

**Bills in other coins.** A bill asking for an asset on another chain (for example USDC on Base) is paid from the shielded balance through a NEAR Intents swap:

1. The daemon asks the swap service for a quote: the ZEC amount, a deposit address, what the recipient gets and a deadline, with one of the wallet's own addresses for refunds.
2. The approval snapshot covers the quote: deposit address, ZEC amount, destination chain, asset, amount and recipient, refund address, and an expiry no later than the quote's deadline.
3. The agent pays it like any bill: one ZEC payment, once. The swap service converts, bridges and delivers, so the agent needs no gas tokens and no balances on other chains.
4. A new final state, `refunded`, covers a swap that did not complete.

The model sees the asset, the amount and the contact alias ("50 USDC to Studio"); the destination and deposit addresses stay owner-only, like Zcash addresses. Who paid stays hidden; the payout is as public as any transfer on that chain. Addresses on those chains are reusable, so a per-recipient allowance ("this address, up to $20 a day") becomes possible there, which fresh diversified Zcash addresses rule out.

Open before building: whether the swap's ZEC deposit address is transparent (the wallet sends to Orchard only), how far the swap service is trusted for delivery and refunds, quotes that expire before the owner answers (autopilot or a fresh quote), and testing that likely needs mainnet funds.
