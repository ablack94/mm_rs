use anyhow::Result;
use axum::extract::ws::{self, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Generic bidirectional WS relay between a client WebSocket and an upstream URL.
/// No message transformation — pure pass-through.
pub async fn relay_public_ws(client_ws: WebSocket, upstream_url: String) -> Result<()> {
    tracing::info!(url = upstream_url, "Public WS relay: connecting upstream");
    let (upstream_ws, _) = connect_async(&upstream_url).await?;
    tracing::info!("Public WS relay: upstream connected");

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (mut client_write, mut client_read) = client_ws.split();

    let client_to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = client_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    if upstream_write
                        .send(Message::Text(text.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ws::Message::Close(_) => break,
                _ => {}
            }
        }
    });

    let upstream_to_client = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_read.next().await {
            match msg {
                Message::Text(text) => {
                    if client_write
                        .send(ws::Message::Text(text.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Ping(data) => {
                    if client_write
                        .send(ws::Message::Ping(data.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = client_to_upstream => {
            tracing::info!("Public WS relay: client disconnected");
        }
        _ = upstream_to_client => {
            tracing::info!("Public WS relay: upstream disconnected");
        }
    }

    Ok(())
}

/// Intercepting private WS relay. Connects to upstream, relays bidirectionally,
/// but allows the caller to transform messages in both directions via closures.
///
/// `outgoing_transform`: Called for each text message from client → upstream.
///   Returns `Ok(Some(text))` to forward, `Ok(None)` to drop, `Err(rejection)` to send rejection back.
///
/// `incoming_transform`: Called for each text message from upstream → client.
///   Returns `Some(text)` to forward, `None` to drop.
pub async fn relay_private_ws<F, G>(
    client_ws: WebSocket,
    upstream_url: &str,
    outgoing_transform: F,
    incoming_transform: G,
) -> Result<()>
where
    F: Fn(String) -> Result<Option<String>, String> + Send + Sync + 'static,
    G: Fn(String) -> Option<String> + Send + Sync + 'static,
{
    tracing::info!(url = upstream_url, "Private WS relay: connecting upstream");
    let (upstream_ws, _) = connect_async(upstream_url).await?;
    tracing::info!("Private WS relay: upstream connected");

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (client_write, mut client_read) = client_ws.split();
    let client_write = Arc::new(Mutex::new(client_write));

    let outgoing_transform = Arc::new(outgoing_transform);
    let incoming_transform = Arc::new(incoming_transform);

    // Client -> Upstream
    let client_write_for_reject = client_write.clone();
    let client_to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = client_read.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let text_str = text.to_string();
                    match outgoing_transform(text_str) {
                        Ok(Some(transformed)) => {
                            if upstream_write
                                .send(Message::Text(transformed.into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(None) => {
                            // Drop message
                        }
                        Err(rejection) => {
                            // Send rejection back to client
                            let mut writer = client_write_for_reject.lock().await;
                            let _ = writer
                                .send(ws::Message::Text(rejection.into()))
                                .await;
                        }
                    }
                }
                ws::Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Upstream -> Client
    let client_write_for_upstream = client_write.clone();
    let upstream_to_client = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_read.next().await {
            match msg {
                Message::Text(text) => {
                    let text_str = text.to_string();
                    if let Some(transformed) = incoming_transform(text_str) {
                        let mut writer = client_write_for_upstream.lock().await;
                        if writer
                            .send(ws::Message::Text(transformed.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Message::Ping(data) => {
                    let mut writer = client_write_for_upstream.lock().await;
                    if writer
                        .send(ws::Message::Ping(data.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = client_to_upstream => {
            tracing::info!("Private WS relay: client disconnected");
        }
        _ = upstream_to_client => {
            tracing::info!("Private WS relay: upstream disconnected");
        }
    }

    Ok(())
}
