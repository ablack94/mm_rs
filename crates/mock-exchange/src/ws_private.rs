use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::messages;
use crate::orders::{Order, OrderStatus, OrderType, Side};
use crate::state::ExchangeState;

/// Handle a private WebSocket connection.
/// Processes order commands from the client and relays execution reports.
pub async fn handle_private_ws(
    socket: WebSocket,
    state: Arc<RwLock<ExchangeState>>,
    mut exec_rx: broadcast::Receiver<String>,
) {
    let (mut ws_write, mut ws_read) = socket.split();

    // Channel for sending messages from the command handler to the writer
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let out_tx_exec = out_tx.clone();

    // Writer task: sends queued messages + relays execution broadcasts
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(msg) = out_rx.recv() => {
                    if ws_write.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                result = exec_rx.recv() => {
                    match result {
                        Ok(msg) => {
                            if ws_write.send(Message::Text(msg.into())).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });

    // Reader task: process incoming commands
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_read.next().await {
            if let Message::Text(text) = msg {
                let text_str = text.to_string();
                process_private_message(&text_str, &state, &out_tx_exec).await;
            }
        }
    });

    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}

/// Process a single incoming private WS message.
/// Accepts both ProxyCommand format (cmd field) and legacy format (method field).
async fn process_private_message(
    raw: &str,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(raw, "Failed to parse private WS message");
            return;
        }
    };

    // Support both new ProxyCommand format ("cmd") and legacy format ("method")
    let cmd = match v.get("cmd").or_else(|| v.get("method")).and_then(|m| m.as_str()) {
        Some(m) => m,
        None => {
            tracing::debug!(raw, "No cmd/method in message");
            return;
        }
    };

    match cmd {
        "subscribe" => handle_subscribe(&v, state, out_tx).await,
        "place_order" | "add_order" => handle_add_order(&v, state, out_tx).await,
        "amend_order" => handle_amend_order(&v, state, out_tx).await,
        "cancel_orders" | "cancel_order" => handle_cancel_order(&v, state, out_tx).await,
        "cancel_all" => handle_cancel_all(&v, state, out_tx).await,
        "set_dms" | "cancel_all_orders_after" => handle_dms(&v, out_tx).await,
        "ping" => handle_ping(&v, out_tx).await,
        other => {
            tracing::warn!(cmd = other, "Unknown private WS command");
        }
    }
}

async fn handle_subscribe(
    v: &serde_json::Value,
    _state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    // New format: {"cmd":"subscribe","channel":"executions"}
    // Old format: {"method":"subscribe","params":{"channel":"executions"}}
    let channel = v.get("channel")
        .or_else(|| v.get("params").and_then(|p| p.get("channel")))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    if channel == "executions" || channel == "book" {
        let confirm = messages::subscribe_confirm(channel);
        let _ = out_tx
            .send(serde_json::to_string(&confirm).unwrap())
            .await;
        tracing::info!(channel, "Subscription confirmed");
    }
}

async fn handle_add_order(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);
    // New format: fields at top level; old format: nested in params
    let params = v.get("params").unwrap_or(v);

    let symbol = params.get("symbol").or_else(|| v.get("symbol"))
        .and_then(|s| s.as_str()).unwrap_or("").to_string();
    let side_str = params.get("side").or_else(|| v.get("side"))
        .and_then(|s| s.as_str()).unwrap_or("buy");
    let side = if side_str == "sell" { Side::Sell } else { Side::Buy };

    let order_type_str = params.get("order_type").or_else(|| v.get("order_type"))
        .and_then(|s| s.as_str()).unwrap_or("limit");
    let is_market = v.get("market").and_then(|m| m.as_bool()).unwrap_or(false);
    let order_type = if order_type_str == "market" || is_market {
        OrderType::Market
    } else {
        OrderType::Limit
    };

    // New format: "price"/"qty"; old format: "limit_price"/"order_qty"
    let price = params.get("price").or_else(|| params.get("limit_price"))
        .or_else(|| v.get("price"))
        .and_then(|p| p.as_f64()).unwrap_or(0.0);
    let qty = params.get("qty").or_else(|| params.get("order_qty"))
        .or_else(|| v.get("qty"))
        .and_then(|q| q.as_f64()).unwrap_or(0.0);
    let cl_ord_id = params.get("cl_ord_id").or_else(|| v.get("cl_ord_id"))
        .and_then(|s| s.as_str()).unwrap_or("").to_string();
    let post_only = params.get("post_only").or_else(|| v.get("post_only"))
        .and_then(|b| b.as_bool()).unwrap_or(false);

    // Validate
    if symbol.is_empty() || cl_ord_id.is_empty() || qty <= 0.0 {
        let resp = messages::order_rejected(req_id, &cl_ord_id, "Invalid order parameters");
        let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
        return;
    }

    let mut st = state.write().await;

    // Check if pair exists
    if !st.books.contains_key(&symbol) {
        let resp = messages::order_rejected(
            req_id,
            &cl_ord_id,
            &format!("Unknown symbol: {}", symbol),
        );
        let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
        return;
    }

    let order_id = st.alloc_order_id();

    let order = Order {
        order_id: order_id.clone(),
        cl_ord_id: cl_ord_id.clone(),
        symbol: symbol.clone(),
        side,
        order_type,
        price,
        qty,
        filled_qty: 0.0,
        status: OrderStatus::New,
        post_only,
    };

    // Check for immediate fill (crossing the book)
    let book = st.books.get(&symbol).unwrap();
    let pd = crate::state::price_decimals_for(book.mid_price());
    let round = |v: f64| -> f64 {
        let factor = 10f64.powi(pd as i32);
        (v * factor).round() / factor
    };
    let rounded_ask = round(book.best_ask().unwrap_or(0.0));
    let rounded_bid = round(book.best_bid().unwrap_or(0.0));

    let should_fill_immediately = match (order_type, side) {
        (OrderType::Market, _) => true,
        (OrderType::Limit, Side::Buy) => price >= rounded_ask && rounded_ask > 0.0,
        (OrderType::Limit, Side::Sell) => price <= rounded_bid && rounded_bid > 0.0,
    };

    if should_fill_immediately {
        let fill_price = match (order_type, side) {
            (OrderType::Market, Side::Buy) => book.best_ask().unwrap_or(0.0),
            (OrderType::Market, Side::Sell) => book.best_bid().unwrap_or(0.0),
            (OrderType::Limit, Side::Buy) => rounded_ask,
            (OrderType::Limit, Side::Sell) => rounded_bid,
        };

        // If post_only and would cross, reject instead of filling
        if post_only {
            let cancel_msg = messages::order_cancelled(&cl_ord_id, "post_only_crossing", &symbol);
            let _ = out_tx
                .send(serde_json::to_string(&cancel_msg).unwrap())
                .await;
            tracing::info!(
                cl_ord_id = cl_ord_id,
                symbol = symbol,
                order_price = price,
                order_side = side_str,
                rounded_ask = rounded_ask,
                rounded_bid = rounded_bid,
                raw_ask = book.best_ask().unwrap_or(0.0),
                raw_bid = book.best_bid().unwrap_or(0.0),
                mid = book.mid_price(),
                price_decimals = pd,
                "Post-only order would cross book, cancelled"
            );
            return;
        }

        // Taker fill
        let taker_fee_pct = 0.40 / 100.0;
        let cost = fill_price * qty;
        let fee = cost * taker_fee_pct;

        let mut order = order;
        order.filled_qty = qty;
        order.status = OrderStatus::Filled;

        // Update balances
        update_balances(&mut st.balances, &symbol, side, qty, fill_price, fee);

        // Send order accepted
        let ack = messages::order_accepted(req_id, &order_id, &cl_ord_id);
        let _ = out_tx.send(serde_json::to_string(&ack).unwrap()).await;

        // Send fill event
        let fill = messages::fill_event(&order, qty, fill_price, fee, false);
        let _ = out_tx.send(serde_json::to_string(&fill).unwrap()).await;

        tracing::info!(
            cl_ord_id = cl_ord_id,
            symbol = symbol,
            side = side_str,
            price = fill_price,
            qty = qty,
            fee = fee,
            "Immediate taker fill"
        );

        // Store the filled order
        st.orders.insert(order);
    } else {
        // Send order accepted and rest the order
        let ack = messages::order_accepted(req_id, &order_id, &cl_ord_id);
        let _ = out_tx.send(serde_json::to_string(&ack).unwrap()).await;

        tracing::info!(
            cl_ord_id = cl_ord_id,
            symbol = symbol,
            side = side_str,
            price = price,
            qty = qty,
            rounded_ask = rounded_ask,
            rounded_bid = rounded_bid,
            mid = book.mid_price(),
            price_decimals = pd,
            "Order resting on book"
        );

        st.orders.insert(order);
    }
}

async fn handle_amend_order(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);
    let params = v.get("params").unwrap_or(v);
    let cl_ord_id = params.get("cl_ord_id").or_else(|| v.get("cl_ord_id"))
        .and_then(|s| s.as_str()).unwrap_or("").to_string();

    let mut st = state.write().await;

    if let Some(order) = st.orders.get_mut(&cl_ord_id) {
        // Update price if provided (new: "price", old: "limit_price")
        if let Some(price) = params.get("price").or_else(|| params.get("limit_price"))
            .or_else(|| v.get("price"))
            .and_then(|p| p.as_f64())
        {
            order.price = price;
        }
        // Update qty if provided (new: "qty", old: "order_qty")
        if let Some(qty) = params.get("qty").or_else(|| params.get("order_qty"))
            .or_else(|| v.get("qty"))
            .and_then(|q| q.as_f64())
        {
            order.qty = qty;
        }

        let resp = messages::command_ack(req_id, "amend_order");
        let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;

        tracing::info!(cl_ord_id = cl_ord_id, "Order amended");
    } else {
        let resp = messages::order_rejected(req_id, &cl_ord_id, "Order not found");
        let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
    }
}

async fn handle_cancel_order(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);
    let params = v.get("params").unwrap_or(v);

    // New format: "cl_ord_ids" array at top level
    // Old format: "cl_ord_id" array in params
    let cl_ord_ids: Vec<String> = params.get("cl_ord_ids")
        .or_else(|| v.get("cl_ord_ids"))
        .or_else(|| params.get("cl_ord_id"))
        .and_then(|ids| ids.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut st = state.write().await;

    for cl_ord_id in &cl_ord_ids {
        if let Some(order) = st.orders.get_mut(cl_ord_id) {
            order.status = OrderStatus::Canceled;
            let symbol = order.symbol.clone();

            // Send cancel event
            let cancel = messages::order_cancelled(cl_ord_id, "user_requested", &symbol);
            let _ = out_tx
                .send(serde_json::to_string(&cancel).unwrap())
                .await;

            tracing::info!(cl_ord_id = cl_ord_id, "Order cancelled");
        }
    }

    // Send ack
    let resp = messages::command_ack(req_id, "cancel_orders");
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
}

async fn handle_cancel_all(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);

    let mut st = state.write().await;

    // Collect orders to cancel
    let open_orders: Vec<Order> = st
        .orders
        .all_open_orders()
        .into_iter()
        .cloned()
        .collect();

    let count = open_orders.len();

    for order in &open_orders {
        if let Some(o) = st.orders.get_mut(&order.cl_ord_id) {
            o.status = OrderStatus::Canceled;
        }
        let cancel = messages::order_cancelled(&order.cl_ord_id, "cancel_all", &order.symbol);
        let _ = out_tx
            .send(serde_json::to_string(&cancel).unwrap())
            .await;
    }

    let resp = messages::command_ack(req_id, "cancel_all");
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;

    tracing::info!(count = count, "Cancel all orders");
}

async fn handle_dms(
    v: &serde_json::Value,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);

    // Just acknowledge -- we don't implement the actual timer
    let resp = messages::command_ack(req_id, "set_dms");
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;

    tracing::debug!(req_id = req_id, "set_dms acknowledged");
}

async fn handle_ping(v: &serde_json::Value, out_tx: &mpsc::Sender<String>) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);
    let resp = messages::pong_response(req_id);
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
}

/// Update balances after a fill.
fn update_balances(
    balances: &mut std::collections::HashMap<String, f64>,
    symbol: &str,
    side: Side,
    qty: f64,
    price: f64,
    fee: f64,
) {
    crate::state::update_balances(balances, symbol, side, qty, price, fee);
}
