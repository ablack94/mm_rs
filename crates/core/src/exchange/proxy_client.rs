use anyhow::{bail, Result};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;

use trading_primitives::Ticker;
use crate::traits::ExchangeClient;
use crate::types::PairInfo;
use crate::types::ticker::TickerData;

/// Exchange client that routes requests through the proxy.
/// The proxy holds the actual API keys and signs requests.
pub struct ProxyClient {
    proxy_base_url: String,
    proxy_token: String,
    client: reqwest::Client,
}

impl ProxyClient {
    pub fn new(proxy_base_url: String, proxy_token: String) -> Self {
        Self {
            proxy_base_url,
            proxy_token,
            client: reqwest::Client::new(),
        }
    }

    /// Send a POST to the proxy, which forwards to the exchange's private API.
    async fn proxy_post(&self, path: &str, body: &str) -> Result<serde_json::Value> {
        let resp: serde_json::Value = self
            .client
            .post(format!("{}{}", self.proxy_base_url, path))
            .header("Authorization", format!("Bearer {}", self.proxy_token))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.to_string())
            .send()
            .await?
            .json()
            .await?;
        check_error(&resp)?;
        Ok(resp)
    }

    /// Send a GET to the proxy, which forwards to the exchange's public API.
    async fn proxy_get(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.proxy_base_url, path);
        let http_resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.proxy_token))
            .send()
            .await?;

        let status = http_resp.status();
        let resp: serde_json::Value = http_resp.json().await?;

        if !status.is_success() {
            let preview: String = resp.to_string().chars().take(200).collect();
            bail!("Proxy GET {} returned {}: {}", path, status, preview);
        }

        Ok(resp)
    }
}

#[async_trait]
impl ExchangeClient for ProxyClient {
    async fn get_ws_token(&self) -> Result<String> {
        // In proxy mode, the WS proxy handles token injection.
        // Return a placeholder — the proxy will replace it.
        Ok("proxy-managed".to_string())
    }

    async fn get_pair_info(&self, target_symbols: &[Ticker]) -> Result<HashMap<Ticker, PairInfo>> {
        if target_symbols.is_empty() {
            return Ok(HashMap::new());
        }

        let resp = self.proxy_get("/0/public/AssetPairs").await?;
        check_error(&resp)?;

        let result = resp["result"]
            .as_object()
            .ok_or_else(|| {
                // Log the actual response for debugging
                let preview: String = resp.to_string().chars().take(200).collect();
                anyhow::anyhow!("Invalid AssetPairs response (no 'result' object): {}", preview)
            })?;

        let mut ws_lookup: HashMap<String, (&str, &serde_json::Value)> = HashMap::new();
        for (key, info) in result {
            if let Some(wsname) = info["wsname"].as_str() {
                ws_lookup.insert(wsname.to_string(), (key.as_str(), info));
            }
        }

        let mut pairs = HashMap::new();
        for symbol in target_symbols {
            if let Some((rest_key, info)) = ws_lookup.get(&symbol.to_string()) {
                let fees_maker = info["fees_maker"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|f| f.get(1))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.26)
                    / 100.0;

                let base_asset = info["base"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();

                pairs.insert(
                    symbol.clone(),
                    PairInfo {
                        pair: symbol.clone(),
                        rest_key: rest_key.to_string(),
                        min_order_qty: parse_decimal_field(info, "ordermin"),
                        min_cost: parse_decimal_field(info, "costmin"),
                        price_decimals: info["pair_decimals"].as_u64().unwrap_or(8) as u32,
                        qty_decimals: info["lot_decimals"].as_u64().unwrap_or(8) as u32,
                        maker_fee_pct: Decimal::try_from(fees_maker).unwrap_or_default(),
                        exchange_base_asset: base_asset,
                    },
                );
            } else {
                tracing::warn!(pair = %symbol, "Pair not found in AssetPairs");
            }
        }

        Ok(pairs)
    }

    async fn get_tickers(&self, pair_info: &HashMap<Ticker, PairInfo>) -> Result<HashMap<Ticker, TickerData>> {
        let rest_keys: Vec<&str> = pair_info.values().map(|pi| pi.rest_key.as_str()).collect();
        let pair_param = rest_keys.join(",");

        let resp = self
            .proxy_get(&format!("/0/public/Ticker?pair={}", pair_param))
            .await?;
        check_error(&resp)?;

        let result = resp["result"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Invalid Ticker response"))?;

        let rest_to_ws: HashMap<&str, &Ticker> = pair_info
            .iter()
            .map(|(ws_sym, pi)| (pi.rest_key.as_str(), ws_sym))
            .collect();

        let mut tickers = HashMap::new();
        for (rest_key, data) in result {
            let ws_symbol = match rest_to_ws.get(rest_key.as_str()) {
                Some(t) => (*t).clone(),
                None => continue,
            };

            let open = data["o"]
                .as_str()
                .and_then(|s| s.parse::<Decimal>().ok())
                .unwrap_or_default();
            let close = data["c"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Decimal>().ok())
                .unwrap_or_default();
            let volume_24h = data["v"]
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Decimal>().ok())
                .unwrap_or_default();

            let change_pct = if !open.is_zero() {
                (close - open) / open * Decimal::from(100)
            } else {
                Decimal::ZERO
            };

            tickers.insert(ws_symbol, TickerData {
                open,
                close,
                volume_24h,
                change_pct,
            });
        }

        Ok(tickers)
    }

    async fn get_balances(&self) -> Result<HashMap<String, Decimal>> {
        let resp = self.proxy_post("/0/private/Balance", "").await?;

        let result = resp["result"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Invalid Balance response"))?;

        let mut balances = HashMap::new();
        for (asset, value) in result {
            if let Some(s) = value.as_str() {
                if let Ok(bal) = s.parse::<Decimal>() {
                    balances.insert(asset.clone(), bal);
                }
            }
        }

        Ok(balances)
    }

}

fn check_error(resp: &serde_json::Value) -> Result<()> {
    if let Some(errors) = resp["error"].as_array() {
        if !errors.is_empty() {
            bail!("Exchange API error: {:?}", errors);
        }
    }
    Ok(())
}

fn parse_decimal_field(info: &serde_json::Value, field: &str) -> Decimal {
    info[field]
        .as_str()
        .and_then(|s| s.parse::<Decimal>().ok())
        .unwrap_or_default()
}
