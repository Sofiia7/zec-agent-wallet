//! MCP server over stdio. It holds no keys and no wallet data: every tool is a call
//! to the daemon's agent API, and every response is checked again for anything that
//! looks like an address, key or txid before it is written to stdout.
//!
//! The protocol layer is deliberately small (newline-delimited JSON-RPC 2.0) so that
//! every byte that can reach the model is produced here.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::dto;

const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_LINE: usize = 1 << 20;

pub struct Options {
    pub daemon_url: String,
    pub token: String,
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (bool, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let unavailable = || {
            (
                false,
                json!({ "reason_code": "daemon_unavailable", "next_action": "owner_review" }),
            )
        };
        let Ok(res) = req.send().await else {
            return unavailable();
        };
        let ok = res.status().is_success();
        match res.json::<Value>().await {
            Ok(value) => (ok, value),
            Err(_) => unavailable(),
        }
    }
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str], read_only: bool) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": false,
            "openWorldHint": !read_only,
        }
    })
}

fn tools() -> Value {
    let id = |what: &str| json!({ "type": "string", "description": what });
    json!([
        tool("wallet_status",
            "Wallet readiness: network, sync, whether the owner has unlocked it or paused payments, spendable balance and what is left of today's spending limit. Amounts are ZEC strings.",
            json!({}), &[], true),
        tool("list_contacts",
            "Contacts the owner has saved, by alias. Addresses are never shown.",
            json!({}), &[], true),
        tool("list_bills",
            "Incoming bills with payee alias, the owner's label, amount and status (needs_approval, approval_requested, approved, paying, paid, rejected, failed, expired_unpaid).",
            json!({ "status": { "type": "string", "description": "Optional status filter." } }), &[], true),
        tool("request_approval",
            "Ask the owner to approve a bill. The owner decides in their own window; this never approves anything by itself.",
            json!({ "bill_id": id("Bill id from list_bills.") }), &["bill_id"], false),
        tool("pay_bill",
            "Pay a bill the owner has approved, exactly as approved. Safe to retry: repeating the call returns the same payment and never pays twice. If the bill is not approved you get a reason_code and next_action instead.",
            json!({ "bill_id": id("Bill id from list_bills.") }), &["bill_id"], false),
        tool("get_payment",
            "Status of an outgoing payment: preparing, signed, sending, submitted, in_block, confirmed, failed, expired_unpaid or owner_check, with confirmations.",
            json!({ "operation_id": id("operation_id returned by pay_bill.") }), &["operation_id"], true),
        tool("request_payment",
            "Create a payment request (an invoice) for a counterparty to pay. The payment link is delivered by the wallet or by the owner; it is not returned to you. Reuse the same idempotency_key when retrying.",
            json!({
                "amount": { "type": "string", "description": "Amount in ZEC, for example \"0.005\"." },
                "label": { "type": "string", "description": "Short description the counterparty will see, max 80 characters." },
                "contact_id": id("Optional contact to deliver the request to."),
                "idempotency_key": id("Any unique string for this request; reuse it on retry."),
            }), &["amount", "label", "idempotency_key"], false),
        tool("check_request",
            "Whether one of your payment requests has been paid: not_paid, partially_paid, payment_detected (waiting for confirmations), paid or overpaid.",
            json!({ "request_id": id("request_id returned by request_payment.") }), &["request_id"], true),
        tool("list_activity",
            "Recent wallet events with payee alias and amount.",
            json!({ "limit": { "type": "integer", "minimum": 1, "maximum": 50 } }), &[], true),
    ])
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Path segments must be our own ids: a prefix and hex digits.
fn safe_id(id: Option<&str>) -> Option<String> {
    let id = id?;
    let ok = id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    ok.then(|| id.to_string())
}

async fn call_tool(client: &Client, name: &str, args: &Value) -> (bool, Value) {
    use reqwest::Method;
    let bad_args = || {
        (
            false,
            json!({ "reason_code": "invalid_arguments", "next_action": "none" }),
        )
    };
    match name {
        "wallet_status" => client.call(Method::GET, "/status", None).await,
        "list_contacts" => client.call(Method::GET, "/contacts", None).await,
        "list_bills" => {
            let path = match str_arg(args, "status") {
                Some(s) if s.chars().all(|c| c.is_ascii_lowercase() || c == '_') => {
                    format!("/bills?status={s}")
                }
                Some(_) => return bad_args(),
                None => "/bills".to_string(),
            };
            client.call(Method::GET, &path, None).await
        }
        "request_approval" => match safe_id(str_arg(args, "bill_id")) {
            Some(id) => client.call(Method::POST, &format!("/bills/{id}/request-approval"), None).await,
            None => bad_args(),
        },
        "pay_bill" => match safe_id(str_arg(args, "bill_id")) {
            Some(id) => client.call(Method::POST, &format!("/bills/{id}/pay"), None).await,
            None => bad_args(),
        },
        "get_payment" => match safe_id(str_arg(args, "operation_id")) {
            Some(id) => client.call(Method::GET, &format!("/payments/{id}"), None).await,
            None => bad_args(),
        },
        "request_payment" => {
            let (Some(amount), Some(label), Some(key)) = (
                str_arg(args, "amount"),
                str_arg(args, "label"),
                str_arg(args, "idempotency_key"),
            ) else {
                return bad_args();
            };
            let body = json!({
                "amount": amount,
                "label": label,
                "contact_id": str_arg(args, "contact_id"),
                "idempotency_key": key,
            });
            client.call(Method::POST, "/requests", Some(body)).await
        }
        "check_request" => match safe_id(str_arg(args, "request_id")) {
            Some(id) => client.call(Method::GET, &format!("/requests/{id}"), None).await,
            None => bad_args(),
        },
        "list_activity" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 50);
            client.call(Method::GET, &format!("/activity?limit={limit}"), None).await
        }
        _ => (
            false,
            json!({ "reason_code": "unknown_tool", "next_action": "none" }),
        ),
    }
}

/// Wraps a tool result for the model, refusing anything that looks like a leak.
fn tool_result(ok: bool, value: Value) -> Value {
    let (ok, value) = if dto::leaks(&value) {
        (
            false,
            json!({ "reason_code": "internal_error", "next_action": "retry_later" }),
        )
    } else {
        (ok, value)
    };
    let text = serde_json::to_string(&value).unwrap_or_default();
    let mut result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !ok,
    });
    if value.is_object() {
        result["structuredContent"] = value;
    }
    result
}

fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn handle(client: &Client, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    // Notifications (no id) get no response.
    let id = id?;
    Some(match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let version = if SUPPORTED_VERSIONS.contains(&requested) {
                requested
            } else {
                SUPPORTED_VERSIONS[0]
            };
            response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "zec-agent-wallet", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Shielded Zcash wallet. You pay only bills the owner approved, and you never see addresses, memos or transaction ids. Approval happens in the owner's own window, never in this chat: if a bill needs approval, call request_approval and tell the user.",
                }),
            )
        }
        "ping" => response(id, json!({})),
        "tools/list" => response(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let (ok, value) = call_tool(client, name, &args).await;
            response(id, tool_result(ok, value))
        }
        "resources/list" => response(id, json!({ "resources": [] })),
        "prompts/list" => response(id, json!({ "prompts": [] })),
        _ => error(id, -32601, "method not found"),
    })
}

pub async fn run(opts: Options) -> anyhow::Result<()> {
    let client = Client {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?,
        base: format!("{}/api/agent", opts.daemon_url.trim_end_matches('/')),
        token: opts.token,
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = if line.len() > MAX_LINE {
            Some(error(Value::Null, -32600, "request too large"))
        } else {
            match serde_json::from_str::<Value>(&line) {
                Ok(msg) => handle(&client, msg).await,
                Err(_) => Some(error(Value::Null, -32700, "parse error")),
            }
        };
        if let Some(reply) = reply {
            let mut out = serde_json::to_vec(&reply)?;
            out.push(b'\n');
            stdout.write_all(&out).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaking_result_is_replaced() {
        let leaked = json!({
            "payee": "Designer",
            "note": "pay utest1s32cjeujn8xj3jy8qemc00xqkw8cps0kxz90earmpl533475y50qg5ams0m9lsnvg2vjc9y0z94aysv05y2dvwsjy0p58mvejyx8awjn"
        });
        let out = tool_result(true, leaked);
        assert_eq!(out["isError"], json!(true));
        assert!(!serde_json::to_string(&out).unwrap().contains("utest1"));
    }

    #[test]
    fn ids_are_validated() {
        assert_eq!(safe_id(Some("bill_0123abcd")), Some("bill_0123abcd".into()));
        assert_eq!(safe_id(Some("../owner/settings")), None);
        assert_eq!(safe_id(Some("bill_1?x=1")), None);
        assert_eq!(safe_id(None), None);
    }
}
