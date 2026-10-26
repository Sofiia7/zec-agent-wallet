#!/usr/bin/env python3
"""A minimal Claude agent that uses the zaw MCP server, through the Claude API.

The agent gets exactly the tools any MCP client gets (wallet_status, list_bills,
pay_bill, ...). It holds no keys and sees no addresses; it can pay only bills the
owner approved in the owner window, exactly as approved.

Setup:
  pip install "anthropic[mcp]"        # Python 3.10+
  set ANTHROPIC_API_KEY=...           # or export on Linux/macOS

Usage (the daemon must be running and unlocked):
  python examples/claude_agent.py --zaw C:\\path\\to\\zaw.exe \\
      --network mainnet --data-dir C:\\path\\to\\wallet --daemon-url http://127.0.0.1:7351 \\
      "Pay the approved bill Mainnet test 1 and tell me the payment status."

  python examples/claude_agent.py --list-tools ...   # check the MCP connection only
"""

import argparse
import asyncio
import json
import sys

import anthropic
from anthropic import AsyncAnthropic
from anthropic.lib.tools.mcp import async_mcp_tool
from mcp import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client

MODEL = "claude-opus-5-5"

SYSTEM = """You are an agent that handles Zcash payments for the owner of a zaw wallet.

The owner approves each bill in the wallet's own window with their password. That
approval is the owner's authorization: when a bill's status is "approved", you are
expected to pay it with pay_bill without asking again. The wallet enforces the rest:
it pays only approved bills, exactly as approved, within the owner's daily limit, and
a repeated pay_bill never pays twice.

You never see addresses, memos, keys or transaction ids, and you must not ask for
them. If a bill is not approved yet, call request_approval and tell the owner. After
paying, check the payment with get_payment and report its status and confirmations."""


def server_params(args: argparse.Namespace) -> StdioServerParameters:
    zaw_args = ["--network", args.network]
    if args.data_dir:
        zaw_args += ["--data-dir", args.data_dir]
    zaw_args += ["mcp", "--daemon-url", args.daemon_url]
    return StdioServerParameters(command=args.zaw, args=zaw_args)


def show(message) -> None:
    for block in message.content:
        if block.type == "text" and block.text.strip():
            print(block.text.strip())
        elif block.type == "tool_use":
            print(f"> {block.name}({json.dumps(block.input)})")


async def run(args: argparse.Namespace) -> int:
    async with stdio_client(server_params(args)) as (read, write):
        async with ClientSession(read, write) as mcp_client:
            await mcp_client.initialize()
            tools = (await mcp_client.list_tools()).tools
            if args.list_tools:
                for t in tools:
                    print(f"{t.name}: {t.description}")
                return 0

            client = AsyncAnthropic()
            runner = client.beta.messages.tool_runner(
                model=MODEL,
                max_tokens=16000,
                system=SYSTEM,
                output_config={"effort": "medium"},
                # On a policy decline the API retries on a fallback model in the same call.
                betas=["server-side-fallback-2026-07-01"],
                fallbacks="default",
                tools=[async_mcp_tool(t, mcp_client) for t in tools],
                messages=[{"role": "user", "content": args.task}],
            )
            last = None
            try:
                async for message in runner:
                    show(message)
                    last = message
            except anthropic.AuthenticationError:
                print("the Claude API rejected the key: set ANTHROPIC_API_KEY", file=sys.stderr)
                return 1
            except anthropic.APIStatusError as e:
                print(f"Claude API error {e.status_code}: {e.message}", file=sys.stderr)
                return 1
            except anthropic.APIConnectionError:
                print("cannot reach the Claude API", file=sys.stderr)
                return 1
            if last is not None and last.stop_reason == "refusal":
                print(f"refused: {last.stop_details}", file=sys.stderr)
                return 1
            return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("task", nargs="?", default="Check the wallet and pay any approved bills.")
    p.add_argument("--zaw", default="zaw", help="path to the zaw binary")
    p.add_argument("--network", default="testnet", choices=["testnet", "mainnet"])
    p.add_argument("--data-dir", help="wallet data directory (default: the OS data directory)")
    p.add_argument("--daemon-url", default="http://127.0.0.1:7341")
    p.add_argument("--list-tools", action="store_true", help="list the MCP tools and exit")
    return asyncio.run(run(p.parse_args()))


if __name__ == "__main__":
    sys.exit(main())
