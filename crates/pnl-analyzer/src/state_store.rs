use anyhow::Result;
use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;

/// HTTP client for the State Store REST API.
pub struct StateStoreClient {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

/// A pair record as returned by the state store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorePairRecord {
    pub symbol: String,
    pub state: String,
    pub config: StorePairConfig,
    pub disabled_reason: Option<String>,
    pub auto_enable_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorePairConfig {
    pub order_size_usd: Option<Decimal>,
    pub max_inventory_usd: Option<Decimal>,
    pub min_spread_bps: Option<Decimal>,
    pub spread_capture_pct: Option<Decimal>,
    pub min_profit_pct: Option<Decimal>,
    pub stop_loss_pct: Option<Decimal>,
    pub take_profit_pct: Option<Decimal>,
}

/// Response from GET /pairs.
#[derive(Debug, Clone, Deserialize)]
pub struct PairsResponse {
    pub pairs: Vec<StorePairRecord>,
}

/// Body for PATCH /pairs/{symbol}.
#[derive(Debug, Clone, Serialize)]
pub struct PatchPairBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<PatchPairConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PatchPairConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_size_usd: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_inventory_usd: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_spread_bps: Option<Decimal>,
}

impl StateStoreClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            client: reqwest::Client::new(),
        }
    }

    /// Encode a pair symbol for use in URL paths (OMG/USD -> OMG-USD).
    fn encode_symbol(symbol: &str) -> String {
        symbol.replace('/', "-")
    }

    /// GET /pairs -- list all pairs from the state store.
    pub async fn get_pairs(&self) -> Result<Vec<StorePairRecord>> {
        let url = format!("{}/pairs", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET /pairs failed: {} - {}", status, body);
        }

        let pairs_resp: PairsResponse = resp.json().await?;
        Ok(pairs_resp.pairs)
    }

    /// PATCH /pairs/{symbol} -- partial update of a pair.
    pub async fn patch_pair(&self, symbol: &str, body: &PatchPairBody) -> Result<()> {
        let encoded = Self::encode_symbol(symbol);
        let url = format!("{}/pairs/{}", self.base_url, encoded);

        let resp = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            anyhow::bail!("PATCH /pairs/{} failed: {} - {}", symbol, status, body_text);
        }

        Ok(())
    }

    /// Check if the state store is reachable.
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}
