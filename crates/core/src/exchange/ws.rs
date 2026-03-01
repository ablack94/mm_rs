use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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
