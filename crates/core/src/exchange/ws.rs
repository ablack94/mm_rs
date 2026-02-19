use anyhow::Result;
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use crate::exchange::messages::*;
use crate::types::*;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Manages a single WebSocket connection, sending raw messages and parsing responses.
pub struct WsConnection {
    write: futures::stream::SplitSink<WsStream, Message>,
    read: futures::stream::SplitStream<WsStream>,
}

/// Write half of a split WsConnection.
pub struct WsWriter {
    write: futures::stream::SplitSink<WsStream, Message>,
}

/// Read half of a split WsConnection.
pub struct WsReader {
    read: futures::stream::SplitStream<WsStream>,
}

impl WsConnection {
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = connect_async(url).await?;
        let (write, read) = ws.split();
        Ok(Self { write, read })
    }

    /// Connect with a bearer token passed via Authorization header.
    /// Used for proxy mode where the WS endpoint requires auth.
    pub async fn connect_with_token(url: &str, token: &str) -> Result<Self> {
        let mut request = url.into_client_request()?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", token).parse()?,
        );
        let (ws, _) = connect_async(request).await?;
        let (write, read) = ws.split();
        Ok(Self { write, read })
    }

    pub async fn send_json(&mut self, msg: &impl serde::Serialize) -> Result<()> {
        let text = serde_json::to_string(msg)?;
        tracing::debug!(msg = &text, "WS send");
        self.write.send(Message::Text(text.into())).await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<String> {
        loop {
            match self.read.next().await {
                Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "WS recv error");
                    return None;
                }
                _ => {}
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.write.close().await;
    }

    /// Split into independent write and read halves for concurrent use.
    pub fn into_split(self) -> (WsWriter, WsReader) {
        (
            WsWriter { write: self.write },
            WsReader { read: self.read },
        )
    }
}

impl WsWriter {
    pub async fn send_raw(&mut self, text: &str) -> Result<()> {
        tracing::debug!(msg = text, "WS send");
        self.write.send(Message::Text(text.to_string().into())).await?;
        Ok(())
    }

    pub async fn close(&mut self) {
        let _ = self.write.close().await;
    }
}

impl WsReader {
    pub async fn recv(&mut self) -> Option<String> {
        loop {
            match self.read.next().await {
                Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "WS recv error");
                    return None;
                }
                _ => {}
            }
        }
    }
}

/// Subscribe to order book updates on a public WS connection.
pub async fn subscribe_book(ws: &mut WsConnection, pairs: &[String], depth: u32) -> Result<()> {
    ws.send_json(&SubscribeBookMsg {
        method: "subscribe",
        params: SubscribeBookParams {
            channel: "book",
            symbol: pairs.to_vec(),
            depth,
        },
    })
    .await
}

/// Subscribe to execution reports on a private WS connection.
pub async fn subscribe_executions(ws: &mut WsConnection, token: &str) -> Result<()> {
    ws.send_json(&SubscribeExecMsg {
        method: "subscribe",
        params: SubscribeExecParams {
            channel: "executions",
            snap_orders: true,
            snap_trades: true,
            ratecounter: true,
            token: token.to_string(),
        },
    })
    .await
}

/// Send a dead man's switch refresh.
pub async fn send_dms(ws: &mut WsConnection, timeout: u64, token: &str, req_id: u64) -> Result<()> {
    ws.send_json(&DmsMsg {
        method: "cancel_all_orders_after",
        params: DmsParams {
            timeout,
            token: token.to_string(),
        },
        req_id,
    })
    .await
}

/// Place a limit or market order.
pub async fn send_add_order(
    ws: &mut WsConnection,
    request: &OrderRequest,
    token: &str,
    req_id: u64,
) -> Result<()> {
    let order_type = if request.market { "market" } else { "limit" };
    ws.send_json(&AddOrderMsg {
        method: "add_order",
        params: AddOrderParams {
            order_type,
            side: request.side.to_string(),
            symbol: request.symbol.clone(),
            limit_price: request.price,
            order_qty: request.qty,
            post_only: if request.market { false } else { request.post_only },
            time_in_force: "gtc",
            cl_ord_id: request.cl_ord_id.clone(),
            token: token.to_string(),
        },
        req_id,
        _priority: if request.urgent { Some("urgent".into()) } else { None },
    })
    .await
}

/// Amend an existing order (preserves queue position).
pub async fn send_amend_order(
    ws: &mut WsConnection,
    cl_ord_id: &str,
    price: Option<Decimal>,
    qty: Option<Decimal>,
    token: &str,
    req_id: u64,
) -> Result<()> {
    ws.send_json(&AmendOrderMsg {
        method: "amend_order",
        params: AmendOrderParams {
            cl_ord_id: cl_ord_id.to_string(),
            limit_price: price,
            order_qty: qty,
            post_only: true,
            token: token.to_string(),
        },
        req_id,
        _priority: None,
    })
    .await
}

/// Cancel orders by cl_ord_id.
pub async fn send_cancel(
    ws: &mut WsConnection,
    cl_ord_ids: &[String],
    token: &str,
    req_id: u64,
) -> Result<()> {
    ws.send_json(&CancelOrderMsg {
        method: "cancel_order",
        params: CancelOrderParams {
            cl_ord_id: cl_ord_ids.to_vec(),
            token: token.to_string(),
        },
        req_id,
        _priority: None,
    })
    .await
}

/// Cancel all orders.
pub async fn send_cancel_all(ws: &mut WsConnection, token: &str, req_id: u64) -> Result<()> {
    ws.send_json(&CancelAllMsg {
        method: "cancel_all",
        params: CancelAllParams {
            token: token.to_string(),
        },
        req_id,
    })
    .await
}

/// Send a ping to keep the connection alive.
pub async fn send_ping(ws: &mut WsConnection, req_id: u64) -> Result<()> {
    ws.send_json(&PingMsg {
        method: "ping",
        req_id,
    })
    .await
}
