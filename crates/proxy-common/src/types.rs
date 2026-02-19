use serde::{Deserialize, Serialize};

/// REST proxy shared state fields.
/// Exchange-specific proxies embed this and add their own fields.
pub struct CommonProxyConfig {
    pub proxy_token: String,
    pub client: reqwest::Client,
}

/// Health check response.
#[derive(Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

impl HealthResponse {
    pub fn ok() -> Self {
        Self {
            status: "ok".to_string(),
        }
    }
}
