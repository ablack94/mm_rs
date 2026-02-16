use anyhow::{bail, Result};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use crate::config::ExchangeConfig;
use crate::exchange::auth::sign_request;
use crate::types::PairInfo;

pub struct KrakenRest {
    config: ExchangeConfig,
    client: reqwest::Client,
}

impl KrakenRest {
    pub fn new(config: ExchangeConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// Get WebSocket authentication token.
    pub async fn get_ws_token(&self) -> Result<String> {
        let urlpath = "/0/private/GetWebSocketsToken";
        let nonce = millis_nonce();
        let post_data = format!("nonce={}", nonce);
        let signature = sign_request(urlpath, &nonce, &post_data, &self.config.api_secret)?;

        let resp: serde_json::Value = self
            .client
            .post(format!("{}{}", self.config.rest_base_url, urlpath))
            .header("API-Key", &self.config.api_key)
            .header("API-Sign", &signature)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(post_data)
            .send()
            .await?
            .json()
            .await?;

        check_error(&resp)?;
        resp["result"]["token"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("No token in response"))
    }

    /// Fetch info for all asset pairs and return the ones matching our target symbols.
    pub async fn get_pair_info(&self, target_symbols: &[String]) -> Result<HashMap<String, PairInfo>> {
        let resp: serde_json::Value = self
            .client
            .get(format!("{}/0/public/AssetPairs", self.config.rest_base_url))
            .send()
            .await?
            .json()
            .await?;

        check_error(&resp)?;

        let result = resp["result"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Invalid AssetPairs response"))?;

        // Build wsname -> info lookup
        let mut ws_lookup: HashMap<String, (&str, &serde_json::Value)> = HashMap::new();
        for (key, info) in result {
            if let Some(wsname) = info["wsname"].as_str() {
                ws_lookup.insert(wsname.to_string(), (key.as_str(), info));
            }
        }

        let mut pairs = HashMap::new();
        for symbol in target_symbols {
            if let Some((rest_key, info)) = ws_lookup.get(symbol) {
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
                        symbol: symbol.clone(),
                        rest_key: rest_key.to_string(),
                        min_order_qty: parse_decimal_field(info, "ordermin"),
                        min_cost: parse_decimal_field(info, "costmin"),
                        price_decimals: info["pair_decimals"].as_u64().unwrap_or(8) as u32,
                        qty_decimals: info["lot_decimals"].as_u64().unwrap_or(8) as u32,
                        maker_fee_pct: Decimal::try_from(fees_maker).unwrap_or_default(),
                        base_asset,
                    },
                );
            } else {
                tracing::warn!(symbol, "Pair not found in AssetPairs");
            }
        }

        Ok(pairs)
    }

    /// Fetch ticker data for all pairs in a single batch request.
    /// Returns 24h open/close/volume and computed change_pct for each pair.
    pub async fn get_tickers(&self, pair_info: &HashMap<String, PairInfo>) -> Result<HashMap<String, TickerData>> {
        let rest_keys: Vec<&str> = pair_info.values().map(|pi| pi.rest_key.as_str()).collect();
        let pair_param = rest_keys.join(",");

        let resp: serde_json::Value = self
            .client
            .get(format!(
                "{}/0/public/Ticker?pair={}",
                self.config.rest_base_url, pair_param
            ))
            .send()
            .await?
            .json()
            .await?;

        check_error(&resp)?;

        let result = resp["result"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Invalid Ticker response"))?;

        // Build rest_key → ws_symbol reverse lookup
        let rest_to_ws: HashMap<&str, &str> = pair_info
            .iter()
            .map(|(ws_sym, pi)| (pi.rest_key.as_str(), ws_sym.as_str()))
            .collect();

        let mut tickers = HashMap::new();
        for (rest_key, data) in result {
            let ws_symbol = match rest_to_ws.get(rest_key.as_str()) {
                Some(s) => s.to_string(),
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

    /// Fetch account balances from Kraken.
    /// Returns a map of asset code (e.g. "HOUSE", "ZUSD") to balance.
    pub async fn get_balances(&self) -> Result<HashMap<String, Decimal>> {
        let urlpath = "/0/private/Balance";
        let nonce = millis_nonce();
        let post_data = format!("nonce={}", nonce);
        let signature = sign_request(urlpath, &nonce, &post_data, &self.config.api_secret)?;

        let resp: serde_json::Value = self
            .client
            .post(format!("{}{}", self.config.rest_base_url, urlpath))
            .header("API-Key", &self.config.api_key)
            .header("API-Sign", &signature)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(post_data)
            .send()
            .await?
            .json()
            .await?;

        check_error(&resp)?;

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

#[derive(Debug, Clone)]
pub struct TickerData {
    pub open: Decimal,
    pub close: Decimal,
    pub volume_24h: Decimal,
    pub change_pct: Decimal,
}

fn millis_nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string()
}

fn check_error(resp: &serde_json::Value) -> Result<()> {
    if let Some(errors) = resp["error"].as_array() {
        if !errors.is_empty() {
            bail!("Kraken API error: {:?}", errors);
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
