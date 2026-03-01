use chrono::Utc;
use serde_json::{json, Value};

use crate::orderbook::OrderBook;
use crate::orders::Order;
use crate::state::{price_decimals_for, qty_decimals_for};

// ─── Book messages (ProxyEvent format) ───

/// Build a book snapshot message for a symbol.
pub fn book_snapshot(symbol: &str, book: &OrderBook) -> Value {
    let mid = book.mid_price();
    let pd = price_decimals_for(mid);
    let qd = qty_decimals_for(mid);

    // Bids: descending (best first) — BTreeMap is ascending, so reverse
    let bids: Vec<Value> = book
        .bids
        .iter()
        .rev()
        .map(|(price, qty)| {
            json!({
                "price": round_f64(price.into_inner(), pd),
                "qty": round_f64(*qty, qd),
            })
        })
        .collect();

    // Asks: ascending (best first) — BTreeMap natural order
    let asks: Vec<Value> = book
        .asks
        .iter()
        .map(|(price, qty)| {
            json!({
                "price": round_f64(price.into_inner(), pd),
                "qty": round_f64(*qty, qd),
            })
        })
        .collect();

    // Debug: log what we're sending
    if !bids.is_empty() && !asks.is_empty() {
        tracing::debug!(
            symbol,
            mid,
            best_bid_raw = book.best_bid(),
            best_ask_raw = book.best_ask(),
            best_bid_rounded = %bids[0]["price"],
            best_ask_rounded = %asks[0]["price"],
            price_decimals = pd,
            qty_decimals = qd,
            "Sending book snapshot"
        );
    }

    json!({
        "event": "book_snapshot",
        "symbol": symbol,
        "bids": bids,
        "asks": asks,
    })
}

/// Build a book update message (sends top 2-3 levels on each side that changed).
#[allow(dead_code)]
pub fn book_update(symbol: &str, book: &OrderBook) -> Value {
    let mid = book.mid_price();
    let pd = price_decimals_for(mid);
    let qd = qty_decimals_for(mid);

    // Top 3 bids (descending — best first)
    let bids: Vec<Value> = book
        .bids
        .iter()
        .rev()
        .take(3)
        .map(|(price, qty)| {
            json!({
                "price": round_f64(price.into_inner(), pd),
                "qty": round_f64(*qty, qd),
            })
        })
        .collect();

    // Top 3 asks (ascending — best first)
    let asks: Vec<Value> = book
        .asks
        .iter()
        .take(3)
        .map(|(price, qty)| {
            json!({
                "price": round_f64(price.into_inner(), pd),
                "qty": round_f64(*qty, qd),
            })
        })
        .collect();

    json!({
        "event": "book_update",
        "symbol": symbol,
        "bids": bids,
        "asks": asks,
    })
}

// ─── Subscription confirmations ───

pub fn subscribe_confirm(channel: &str) -> Value {
    json!({
        "event": "subscribed",
        "channel": channel,
    })
}

// ─── Order responses ───

pub fn order_accepted(req_id: u64, order_id: &str, cl_ord_id: &str) -> Value {
    json!({
        "event": "order_accepted",
        "req_id": req_id,
        "cl_ord_id": cl_ord_id,
        "order_id": order_id,
    })
}

pub fn order_rejected(req_id: u64, cl_ord_id: &str, error: &str) -> Value {
    json!({
        "event": "order_rejected",
        "req_id": req_id,
        "cl_ord_id": cl_ord_id,
        "error": error,
    })
}

pub fn order_cancelled(cl_ord_id: &str, reason: &str, symbol: &str) -> Value {
    json!({
        "event": "order_cancelled",
        "cl_ord_id": cl_ord_id,
        "reason": reason,
        "symbol": symbol,
    })
}

pub fn command_ack(req_id: u64, cmd: &str) -> Value {
    json!({
        "event": "command_ack",
        "req_id": req_id,
        "cmd": cmd,
    })
}

pub fn pong_response(req_id: u64) -> Value {
    json!({
        "event": "pong",
        "req_id": req_id,
    })
}

// ─── Fill events ───

pub fn fill_event(
    order: &Order,
    last_qty: f64,
    last_price: f64,
    fee: f64,
    is_maker: bool,
) -> Value {
    let is_fully_filled = order.filled_qty + last_qty >= order.qty;
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();

    json!({
        "event": "fill",
        "order_id": order.order_id,
        "cl_ord_id": order.cl_ord_id,
        "symbol": order.symbol,
        "side": order.side.to_string(),
        "qty": last_qty,
        "price": last_price,
        "fee": fee,
        "is_maker": is_maker,
        "timestamp": ts,
        "is_fully_filled": is_fully_filled,
    })
}

/// Heartbeat message.
pub fn heartbeat() -> Value {
    json!({"event": "heartbeat"})
}

// ─── Utility ───

fn round_f64(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orderbook::OrderBook;
    use rust_decimal::Decimal;

    /// Verify book_snapshot JSON has correct structure and round-trips through
    /// the same parsing logic the bot uses (f64 → Decimal).
    #[test]
    fn test_book_snapshot_camp_prices() {
        let book = OrderBook::new(0.004, 5.0);
        let snap = book_snapshot("CAMP/USD", &book);

        // Verify new ProxyEvent structure
        assert_eq!(snap["event"], "book_snapshot");
        assert_eq!(snap["symbol"], "CAMP/USD");

        let bids = snap["bids"].as_array().unwrap();
        let asks = snap["asks"].as_array().unwrap();
        assert_eq!(bids.len(), 10);
        assert_eq!(asks.len(), 10);

        // Best bid/ask should be correct
        let best_bid = bids[0]["price"].as_f64().unwrap();
        let best_ask = asks[0]["price"].as_f64().unwrap();

        // With mid=0.004, 5% spread: bid=0.0039, ask=0.0041
        assert!((best_bid - 0.0039).abs() < 0.0001, "best_bid={best_bid}");
        assert!((best_ask - 0.0041).abs() < 0.0001, "best_ask={best_ask}");

        // Verify f64 → Decimal round-trip (same as bot's parse_levels)
        let bid_dec = Decimal::try_from(best_bid).unwrap();
        let ask_dec = Decimal::try_from(best_ask).unwrap();
        let mid = (bid_dec + ask_dec) / Decimal::from(2);

        // Mid should be ~0.004, not 0.0043 or anything else
        let mid_f64: f64 = mid.to_string().parse().unwrap();
        assert!(
            (mid_f64 - 0.004).abs() < 0.001,
            "CAMP mid={mid_f64}, expected ~0.004"
        );
    }

    #[test]
    fn test_book_snapshot_omg_prices() {
        let book = OrderBook::new(0.50, 5.0);
        let snap = book_snapshot("OMG/USD", &book);

        let bids = snap["bids"].as_array().unwrap();
        let asks = snap["asks"].as_array().unwrap();

        let best_bid = bids[0]["price"].as_f64().unwrap();
        let best_ask = asks[0]["price"].as_f64().unwrap();

        // With mid=0.50, 5% spread: bid≈0.4875, ask≈0.5125
        assert!((best_bid - 0.4875).abs() < 0.01, "best_bid={best_bid}");
        assert!((best_ask - 0.5125).abs() < 0.01, "best_ask={best_ask}");

        // Verify round-trip
        let bid_dec = Decimal::try_from(best_bid).unwrap();
        let ask_dec = Decimal::try_from(best_ask).unwrap();
        let mid = (bid_dec + ask_dec) / Decimal::from(2);
        let mid_f64: f64 = mid.to_string().parse().unwrap();

        assert!(
            (mid_f64 - 0.50).abs() < 0.05,
            "OMG mid={mid_f64}, expected ~0.50"
        );
    }

    /// Verify the JSON serialization round-trip: the book snapshot is
    /// serialized to string and parsed back, simulating the WS transport.
    #[test]
    fn test_book_snapshot_json_roundtrip() {
        let book = OrderBook::new(0.004, 5.0);
        let snap = book_snapshot("CAMP/USD", &book);

        // Serialize to string (as WS would)
        let json_str = serde_json::to_string(&snap).unwrap();

        // Parse back (as bot would)
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        let bids = parsed["bids"].as_array().unwrap();
        let asks = parsed["asks"].as_array().unwrap();

        let best_bid = bids[0]["price"].as_f64().unwrap();
        let best_ask = asks[0]["price"].as_f64().unwrap();

        // After serialization round-trip, prices should still be correct
        assert!(
            (best_bid - 0.0039).abs() < 0.0001,
            "roundtrip best_bid={best_bid}"
        );
        assert!(
            (best_ask - 0.0041).abs() < 0.0001,
            "roundtrip best_ask={best_ask}"
        );
    }

    #[test]
    fn test_round_f64_precision() {
        // CAMP-range prices (6 decimals)
        assert_eq!(round_f64(0.003901, 6), 0.003901);
        assert_eq!(round_f64(0.004101, 6), 0.004101);

        // OMG-range prices (5 decimals)
        assert_eq!(round_f64(0.48626, 5), 0.48626);
        assert_eq!(round_f64(0.51157, 5), 0.51157);

        // Edge: rounding should work
        assert_eq!(round_f64(0.0039005, 6), 0.003901);
        assert_eq!(round_f64(0.003900499, 6), 0.0039);
    }

    /// Verify book levels are sorted correctly: bids descending, asks ascending.
    #[test]
    fn test_book_level_ordering() {
        let book = OrderBook::new(0.004, 5.0);
        let snap = book_snapshot("CAMP/USD", &book);

        let bids = snap["bids"].as_array().unwrap();
        let asks = snap["asks"].as_array().unwrap();

        // Bids should be descending (best first)
        for i in 1..bids.len() {
            let prev = bids[i - 1]["price"].as_f64().unwrap();
            let curr = bids[i]["price"].as_f64().unwrap();
            assert!(prev >= curr, "Bids not descending at {i}: {prev} < {curr}");
        }

        // Asks should be ascending (best first)
        for i in 1..asks.len() {
            let prev = asks[i - 1]["price"].as_f64().unwrap();
            let curr = asks[i]["price"].as_f64().unwrap();
            assert!(prev <= curr, "Asks not ascending at {i}: {prev} > {curr}");
        }
    }

    /// Verify the crossing check logic matches what the bot expects.
    #[test]
    fn test_post_only_crossing_check() {
        let book = OrderBook::new(0.004, 5.0);
        let pd = crate::state::price_decimals_for(book.mid_price());
        let factor = 10f64.powi(pd as i32);
        let round = |v: f64| (v * factor).round() / factor;

        let rounded_ask = round(book.best_ask().unwrap());
        let rounded_bid = round(book.best_bid().unwrap());

        // Bot's bid at 50% capture: mid * (1 - spread*0.5/2) ≈ 0.004 * 0.9875 = 0.00395
        let bot_bid = 0.00395;
        assert!(
            bot_bid < rounded_ask,
            "Bot bid {bot_bid} should be BELOW ask {rounded_ask}"
        );

        // Bot's ask: mid * (1 + spread*0.5/2) ≈ 0.004 * 1.0125 = 0.00405
        let bot_ask = 0.00405;
        assert!(
            bot_ask > rounded_bid,
            "Bot ask {bot_ask} should be ABOVE bid {rounded_bid}"
        );
    }
}
