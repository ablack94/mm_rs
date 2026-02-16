"""MCP server for direct Kraken exchange access via proxy.

Provides exchange-level visibility and order control independent of the bot.
Routes through the signing proxy so Claude can inspect balances, orders,
market data, and execute sells for ops/liquidation.

Architecture:
  Claude Code <-stdio-> kraken_mcp.py <-HTTP-> proxy <-HTTPS-> Kraken API

Environment variables:
  PROXY_URL   - Proxy base URL (default: http://localhost:8080)
  PROXY_TOKEN - Bearer token for proxy auth (required)
"""

import os
import json
import httpx
from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.types import Tool, TextContent

PROXY_URL = os.environ.get("PROXY_URL", "http://localhost:8080")
PROXY_TOKEN = os.environ.get("PROXY_TOKEN", "")

server = Server("kraken-exchange")


def _headers() -> dict:
    return {"Authorization": f"Bearer {PROXY_TOKEN}"}


async def _get(path: str) -> dict:
    """GET request to proxy (public endpoints)."""
    async with httpx.AsyncClient() as client:
        r = await client.get(f"{PROXY_URL}{path}", headers=_headers(), timeout=10)
        return r.json()


async def _post(path: str, body: str = "") -> dict:
    """POST request to proxy (private endpoints). Body is form-urlencoded."""
    async with httpx.AsyncClient() as client:
        r = await client.post(
            f"{PROXY_URL}{path}",
            headers={**_headers(), "Content-Type": "application/x-www-form-urlencoded"},
            content=body,
            timeout=10,
        )
        return r.json()


async def _resolve_pair(pair: str) -> dict:
    """Call AssetPairs to get rest key, wsname, and base asset for a pair.

    Accepts either wsname (BTC/USD) or rest key (XXBTZUSD).
    Returns {"rest_key": ..., "wsname": ..., "base": ...} or {"error": ...}.
    """
    data = await _get(f"/0/public/AssetPairs?pair={pair}")
    if data.get("error") and len(data["error"]) > 0:
        return {"error": data["error"]}
    result = data.get("result", {})
    if not result:
        return {"error": f"No pair info returned for {pair}"}
    # AssetPairs returns {rest_key: {info...}} — take the first (only) entry
    rest_key = next(iter(result))
    info = result[rest_key]
    return {
        "rest_key": rest_key,
        "wsname": info.get("wsname", pair),
        "base": info.get("base", ""),
        "quote": info.get("quote", ""),
        "pair_decimals": info.get("pair_decimals", 5),
        "lot_decimals": info.get("lot_decimals", 8),
    }


async def _get_balance_for_asset(asset: str) -> str:
    """Get balance for a specific asset. Returns volume as string."""
    data = await _post("/0/private/Balance")
    if data.get("error") and len(data["error"]) > 0:
        raise Exception(f"Balance error: {data['error']}")
    balances = data.get("result", {})
    vol = balances.get(asset, "0")
    return vol


@server.list_tools()
async def list_tools():
    return [
        # --- Exchange health & info ---
        Tool(
            name="health",
            description="Check if the signing proxy is alive",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="get_asset_pairs",
            description="Get pair info (decimals, minimums, fees). Accepts wsname (BTC/USD) or rest key (XXBTZUSD).",
            inputSchema={
                "type": "object",
                "properties": {
                    "pair": {
                        "type": "string",
                        "description": "Pair name, e.g. XXBTZUSD or BTC/USD",
                    }
                },
                "required": ["pair"],
            },
        ),
        # --- Market data ---
        Tool(
            name="get_ticker",
            description="Get current price, spread, volume for a pair",
            inputSchema={
                "type": "object",
                "properties": {
                    "pair": {
                        "type": "string",
                        "description": "Pair name, e.g. XXBTZUSD",
                    }
                },
                "required": ["pair"],
            },
        ),
        Tool(
            name="get_ohlc",
            description="Get OHLC candle data for a pair",
            inputSchema={
                "type": "object",
                "properties": {
                    "pair": {
                        "type": "string",
                        "description": "Pair name, e.g. XXBTZUSD",
                    },
                    "interval": {
                        "type": "integer",
                        "description": "Candle interval in minutes: 1, 5, 15, 30, 60, 240, 1440, 10080, 21600. Default 60.",
                        "default": 60,
                    },
                },
                "required": ["pair"],
            },
        ),
        # --- Account (private) ---
        Tool(
            name="get_balances",
            description="Get all asset balances on Kraken",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="get_trade_history",
            description="Get recent trade history (fills) from Kraken",
            inputSchema={"type": "object", "properties": {}},
        ),
        Tool(
            name="get_open_orders",
            description="Get all open orders on the exchange",
            inputSchema={"type": "object", "properties": {}},
        ),
        # --- Order management ---
        Tool(
            name="market_sell",
            description="Immediate market sell of full balance for a pair. Auto-fetches balance for volume. Use for emergency exits.",
            inputSchema={
                "type": "object",
                "properties": {
                    "pair": {
                        "type": "string",
                        "description": "Pair name, e.g. XXBTZUSD or BTC/USD",
                    }
                },
                "required": ["pair"],
            },
        ),
        Tool(
            name="limit_sell",
            description="Place a limit sell at top of book (or specified price) for full balance. Auto-fetches balance and best ask if no price given.",
            inputSchema={
                "type": "object",
                "properties": {
                    "pair": {
                        "type": "string",
                        "description": "Pair name, e.g. XXBTZUSD or BTC/USD",
                    },
                    "price": {
                        "type": "string",
                        "description": "Limit price. If omitted, uses current best ask from ticker.",
                    },
                },
                "required": ["pair"],
            },
        ),
        Tool(
            name="cancel_order",
            description="Cancel a specific order by its txid",
            inputSchema={
                "type": "object",
                "properties": {
                    "txid": {
                        "type": "string",
                        "description": "Order transaction ID to cancel",
                    }
                },
                "required": ["txid"],
            },
        ),
    ]


@server.call_tool()
async def call_tool(name: str, arguments: dict):
    try:
        if name == "health":
            result = await _get("/health")

        elif name == "get_asset_pairs":
            pair = arguments["pair"]
            result = await _get(f"/0/public/AssetPairs?pair={pair}")

        elif name == "get_ticker":
            pair = arguments["pair"]
            result = await _get(f"/0/public/Ticker?pair={pair}")

        elif name == "get_ohlc":
            pair = arguments["pair"]
            interval = arguments.get("interval", 60)
            result = await _get(f"/0/public/OHLC?pair={pair}&interval={interval}")

        elif name == "get_balances":
            result = await _post("/0/private/Balance")

        elif name == "get_trade_history":
            result = await _post("/0/private/TradesHistory")

        elif name == "get_open_orders":
            result = await _post("/0/private/OpenOrders")

        elif name == "market_sell":
            result = await _do_market_sell(arguments["pair"])

        elif name == "limit_sell":
            result = await _do_limit_sell(arguments["pair"], arguments.get("price"))

        elif name == "cancel_order":
            txid = arguments["txid"]
            result = await _post("/0/private/CancelOrder", f"txid={txid}")

        else:
            result = {"error": f"Unknown tool: {name}"}

        text = json.dumps(result, indent=2)
        return [TextContent(type="text", text=text)]

    except httpx.ConnectError:
        return [TextContent(type="text", text="Error: Cannot connect to proxy. Is it running?")]
    except Exception as e:
        return [TextContent(type="text", text=f"Error: {e}")]


async def _do_market_sell(pair: str) -> dict:
    """Market sell full balance of base asset for a pair."""
    pair_info = await _resolve_pair(pair)
    if "error" in pair_info:
        return pair_info

    rest_key = pair_info["rest_key"]
    base = pair_info["base"]
    lot_decimals = pair_info["lot_decimals"]

    volume = await _get_balance_for_asset(base)
    vol_f = float(volume)
    if vol_f <= 0:
        return {"error": f"No {base} balance to sell (balance: {volume})"}

    # Truncate to lot_decimals (don't round up)
    volume = f"{vol_f:.{lot_decimals}f}"

    body = f"pair={rest_key}&type=sell&ordertype=market&volume={volume}"
    result = await _post("/0/private/AddOrder", body)
    result["_mcp_meta"] = {
        "action": "market_sell",
        "pair": rest_key,
        "base": base,
        "volume": volume,
    }
    return result


async def _do_limit_sell(pair: str, price: str | None = None) -> dict:
    """Limit sell full balance at specified price or best ask."""
    pair_info = await _resolve_pair(pair)
    if "error" in pair_info:
        return pair_info

    rest_key = pair_info["rest_key"]
    base = pair_info["base"]
    lot_decimals = pair_info["lot_decimals"]
    pair_decimals = pair_info["pair_decimals"]

    volume = await _get_balance_for_asset(base)
    vol_f = float(volume)
    if vol_f <= 0:
        return {"error": f"No {base} balance to sell (balance: {volume})"}

    volume = f"{vol_f:.{lot_decimals}f}"

    if price is None:
        ticker = await _get(f"/0/public/Ticker?pair={rest_key}")
        if ticker.get("error") and len(ticker["error"]) > 0:
            return {"error": f"Ticker error: {ticker['error']}"}
        ticker_result = ticker.get("result", {})
        ticker_data = next(iter(ticker_result.values()), {})
        # 'a' = ask array, first element is price
        ask_price = ticker_data.get("a", [None])[0]
        if ask_price is None:
            return {"error": "Could not fetch best ask price from ticker"}
        price = ask_price

    # Truncate price to pair_decimals
    price_f = float(price)
    price = f"{price_f:.{pair_decimals}f}"

    body = f"pair={rest_key}&type=sell&ordertype=limit&price={price}&volume={volume}"
    result = await _post("/0/private/AddOrder", body)
    result["_mcp_meta"] = {
        "action": "limit_sell",
        "pair": rest_key,
        "base": base,
        "volume": volume,
        "price": price,
    }
    return result


async def main():
    async with stdio_server() as (read_stream, write_stream):
        await server.run(read_stream, write_stream, server.create_initialization_options())


if __name__ == "__main__":
    import asyncio
    asyncio.run(main())
