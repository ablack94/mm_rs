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

    let method = match v["method"].as_str() {
        Some(m) => m,
        None => {
            // Might be a subscription request
            if v["method"].is_null() {
                tracing::debug!(raw, "No method in message");
            }
            return;
        }
    };

    match method {
        "subscribe" => handle_subscribe(&v, state, out_tx).await,
        "add_order" => handle_add_order(&v, state, out_tx).await,
        "amend_order" => handle_amend_order(&v, state, out_tx).await,
        "cancel_order" => handle_cancel_order(&v, state, out_tx).await,
        "cancel_all" => handle_cancel_all(&v, state, out_tx).await,
        "cancel_all_orders_after" => handle_cancel_all_after(&v, state, out_tx).await,
        "ping" => handle_ping(&v, out_tx).await,
        other => {
            tracing::warn!(method = other, "Unknown private WS method");
        }
    }
}

async fn handle_subscribe(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let channel = v["params"]["channel"].as_str().unwrap_or("");
    if channel == "executions" {
        // Send subscription confirmation
        let confirm = messages::subscribe_executions_confirm();
        let _ = out_tx
            .send(serde_json::to_string(&confirm).unwrap())
            .await;

        let snap_trades = v["params"]["snap_trades"].as_bool().unwrap_or(false);
        let snap_orders = v["params"]["snap_orders"].as_bool().unwrap_or(false);

        // Send trades snapshot (empty)
        if snap_trades {
            let snap = messages::executions_snapshot_trades();
            let _ = out_tx
                .send(serde_json::to_string(&snap).unwrap())
                .await;
        }

        // Send orders snapshot
        if snap_orders {
            let st = state.read().await;
            let open_orders: Vec<&Order> = st.orders.all_open_orders();
            let snap = messages::executions_snapshot_orders(&open_orders);
            let _ = out_tx
                .send(serde_json::to_string(&snap).unwrap())
                .await;
        }

        tracing::info!("Executions subscription confirmed");
    }
}

async fn handle_add_order(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);
    let params = &v["params"];

    let symbol = params["symbol"].as_str().unwrap_or("").to_string();
    let side_str = params["side"].as_str().unwrap_or("buy");
    let side = if side_str == "sell" {
        Side::Sell
    } else {
        Side::Buy
    };
    let order_type_str = params["order_type"].as_str().unwrap_or("limit");
    let order_type = if order_type_str == "market" {
        OrderType::Market
    } else {
        OrderType::Limit
    };
    let price = params["limit_price"].as_f64().unwrap_or(0.0);
    let qty = params["order_qty"].as_f64().unwrap_or(0.0);
    let cl_ord_id = params["cl_ord_id"].as_str().unwrap_or("").to_string();
    let post_only = params["post_only"].as_bool().unwrap_or(false);

    // Validate
    if symbol.is_empty() || cl_ord_id.is_empty() || qty <= 0.0 {
        let resp = messages::order_response_error("add_order", req_id, "Invalid order parameters");
        let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
        return;
    }

    let mut st = state.write().await;

    // Check if pair exists
    if !st.books.contains_key(&symbol) {
        let resp = messages::order_response_error(
            "add_order",
            req_id,
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

    // Send success response
    let resp = messages::order_response_success("add_order", req_id, &order_id, &cl_ord_id);
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;

    // Send pending_new exec report
    let pending = messages::exec_report("pending_new", &order, 0.0, 0.0, 0.0, false);
    let _ = out_tx
        .send(serde_json::to_string(&pending).unwrap())
        .await;

    // Check for immediate fill (crossing the book)
    //
    // Use ROUNDED prices (same precision as book update messages) for the
    // crossing check.  The bot computes its limit prices from the rounded
    // book data it receives, so the mock must compare against the same
    // rounded values.  Without this, a sub-tick timing race between the
    // book update broadcast and the add_order arrival can cause the raw
    // (unrounded) ask to sit fractionally below the bot's bid, triggering
    // a spurious post_only cancellation.
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
            (OrderType::Limit, Side::Buy) => rounded_ask,  // fill at rounded ask (taker)
            (OrderType::Limit, Side::Sell) => rounded_bid,  // fill at rounded bid (taker)
        };

        // If post_only and would cross, reject instead of filling
        if post_only {
            let mut order = order;
            order.status = OrderStatus::Canceled;
            let cancel_report =
                messages::exec_report("canceled", &order, 0.0, 0.0, 0.0, false);
            let _ = out_tx
                .send(serde_json::to_string(&cancel_report).unwrap())
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

        // Send new ack
        let new_ack = messages::exec_report("new", &order, 0.0, 0.0, 0.0, false);
        let _ = out_tx
            .send(serde_json::to_string(&new_ack).unwrap())
            .await;

        // Send trade execution
        let trade = messages::exec_report("trade", &order, qty, fill_price, fee, false);
        let _ = out_tx
            .send(serde_json::to_string(&trade).unwrap())
            .await;

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
        // Send new ack and rest the order
        let new_ack = messages::exec_report("new", &order, 0.0, 0.0, 0.0, false);
        let _ = out_tx
            .send(serde_json::to_string(&new_ack).unwrap())
            .await;

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
    let params = &v["params"];
    let cl_ord_id = params["cl_ord_id"].as_str().unwrap_or("").to_string();

    let mut st = state.write().await;

    if let Some(order) = st.orders.get_mut(&cl_ord_id) {
        // Update price if provided
        if let Some(price) = params["limit_price"].as_f64() {
            order.price = price;
        }
        // Update qty if provided
        if let Some(qty) = params["order_qty"].as_f64() {
            order.qty = qty;
        }

        let resp = messages::order_response_success(
            "amend_order",
            req_id,
            &order.order_id.clone(),
            &cl_ord_id,
        );
        let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;

        // Send exec report for the amended order
        let order_clone = order.clone();
        let report = messages::exec_report("new", &order_clone, 0.0, 0.0, 0.0, false);
        let _ = out_tx
            .send(serde_json::to_string(&report).unwrap())
            .await;

        tracing::info!(cl_ord_id = cl_ord_id, "Order amended");
    } else {
        let resp =
            messages::order_response_error("amend_order", req_id, "Order not found");
        let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
    }
}

async fn handle_cancel_order(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);
    let params = &v["params"];

    let cl_ord_ids: Vec<String> = params["cl_ord_id"]
        .as_array()
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
            let order_clone = order.clone();

            // Send cancel exec report
            let report =
                messages::exec_report("canceled", &order_clone, 0.0, 0.0, 0.0, false);
            let _ = out_tx
                .send(serde_json::to_string(&report).unwrap())
                .await;

            tracing::info!(cl_ord_id = cl_ord_id, "Order cancelled");
        }
    }

    // Send success response
    let resp = if let Some(first_id) = cl_ord_ids.first() {
        let order_id = st
            .orders
            .get(first_id)
            .map(|o| o.order_id.clone())
            .unwrap_or_default();
        messages::order_response_success("cancel_order", req_id, &order_id, first_id)
    } else {
        messages::order_response_error("cancel_order", req_id, "No orders specified")
    };
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;
}

async fn handle_cancel_all(
    v: &serde_json::Value,
    state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);

    let mut st = state.write().await;

    // First collect orders to cancel, then cancel them
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
        let report =
            messages::exec_report("canceled", order, 0.0, 0.0, 0.0, false);
        let _ = out_tx
            .send(serde_json::to_string(&report).unwrap())
            .await;
    }

    let resp = messages::cancel_all_response(req_id, count);
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;

    tracing::info!(count = count, "Cancel all orders");
}

async fn handle_cancel_all_after(
    v: &serde_json::Value,
    _state: &Arc<RwLock<ExchangeState>>,
    out_tx: &mpsc::Sender<String>,
) {
    let req_id = v["req_id"].as_u64().unwrap_or(0);
    let _timeout = v["params"]["timeout"].as_u64().unwrap_or(0);

    // Just acknowledge -- we don't implement the actual timer for now
    // In a more complete implementation, we'd set a timer that cancels all
    // orders if not refreshed within the timeout period.
    let resp = messages::cancel_all_orders_after_response(req_id);
    let _ = out_tx.send(serde_json::to_string(&resp).unwrap()).await;

    tracing::debug!(req_id = req_id, "cancel_all_orders_after acknowledged");
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

