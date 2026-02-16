"""MCP server for Kraken Market Making bot.

Wraps the bot's REST API endpoints so Claude can inspect state,
cancel orders, pause/resume quoting, and liquidate positions.

Environment variables:
  BOT_URL       - Bot REST API base URL (default: http://localhost:3030)
  BOT_API_TOKEN - Bearer token for authentication (required)
"""

import os
import json
import httpx
from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.types import Tool, TextContent

BOT_URL = os.environ.get("BOT_URL", "http://localhost:3030")
BOT_API_TOKEN = os.environ.get("BOT_API_TOKEN", "")

server = Server("kraken-mm")


def _headers() -> dict:
    return {"Authorization": f"Bearer {BOT_API_TOKEN}"}


async def _get(path: str) -> str:
    async with httpx.AsyncClient() as client:
        r = await client.get(f"{BOT_URL}{path}", headers=_headers(), timeout=10)
        return r.text


async def _post(path: str) -> str:
    async with httpx.AsyncClient() as client:
        r = await client.post(f"{BOT_URL}{path}", headers=_headers(), timeout=10)
        return r.text


@server.list_tools()
async def list_tools():
    return [
        Tool(
            name="get_state",
            description="Get full bot state (positions, orders, P&L, fees, trade count)",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="get_positions",
            description="Get current inventory positions (symbol, qty, avg_cost)",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="get_orders",
            description="Get all open orders",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="get_trades",
            description="Get recent trades from the CSV log",
            inputSchema={
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Number of recent trades to return (default 50)",
                        "default": 50,
                    }
                },
            },
        ),
        Tool(
            name="get_config",
            description="Get bot configuration (secrets redacted)",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="get_ticker",
            description="Get Kraken ticker data for a pair (proxied through bot)",
            inputSchema={
                "type": "object",
                "properties": {
                    "pair": {
                        "type": "string",
                        "description": "Kraken pair name, e.g. CAMPUSD",
                    }
                },
                "required": ["pair"],
            },
        ),
        Tool(
            name="cancel_all",
            description="Cancel ALL open orders on the exchange",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="cancel_order",
            description="Cancel a specific order by client order ID",
            inputSchema={
                "type": "object",
                "properties": {
                    "cl_ord_id": {
                        "type": "string",
                        "description": "Client order ID to cancel",
                    }
                },
                "required": ["cl_ord_id"],
            },
        ),
        Tool(
            name="pause",
            description="Pause quoting: enter sell-only mode for all pairs (no new buys)",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="resume",
            description="Resume normal quoting (exit sell-only mode)",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="liquidate",
            description="Force-sell a position at market (taker order at best bid). Use CAMP_USD format (underscore instead of slash).",
            inputSchema={
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Symbol to liquidate, e.g. CAMP_USD (use underscore instead of slash)",
                    }
                },
                "required": ["symbol"],
            },
        ),
    ]


@server.call_tool()
async def call_tool(name: str, arguments: dict):
    try:
        if name == "get_state":
            result = await _get("/api/state")
        elif name == "get_positions":
            result = await _get("/api/positions")
        elif name == "get_orders":
            result = await _get("/api/orders")
        elif name == "get_trades":
            limit = arguments.get("limit", 50)
            result = await _get(f"/api/trades?limit={limit}")
        elif name == "get_config":
            result = await _get("/api/config")
        elif name == "get_ticker":
            pair = arguments["pair"]
            result = await _get(f"/api/ticker/{pair}")
        elif name == "cancel_all":
            result = await _post("/api/cancel-all")
        elif name == "cancel_order":
            cl_ord_id = arguments["cl_ord_id"]
            result = await _post(f"/api/cancel/{cl_ord_id}")
        elif name == "pause":
            result = await _post("/api/pause")
        elif name == "resume":
            result = await _post("/api/resume")
        elif name == "liquidate":
            symbol = arguments["symbol"]
            result = await _post(f"/api/liquidate/{symbol}")
        else:
            result = json.dumps({"error": f"Unknown tool: {name}"})

        # Pretty-print JSON if possible
        try:
            parsed = json.loads(result)
            result = json.dumps(parsed, indent=2)
        except (json.JSONDecodeError, TypeError):
            pass

        return [TextContent(type="text", text=result)]

    except httpx.ConnectError:
        return [TextContent(type="text", text="Error: Cannot connect to bot. Is it running?")]
    except Exception as e:
        return [TextContent(type="text", text=f"Error: {e}")]


async def main():
    async with stdio_server() as (read_stream, write_stream):
        await server.run(read_stream, write_stream, server.create_initialization_options())


if __name__ == "__main__":
    import asyncio
    asyncio.run(main())
