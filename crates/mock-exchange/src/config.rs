use crate::scenario::ScenarioFile;

/// Configuration parsed from environment variables.
#[derive(Debug, Clone)]
pub struct MockConfig {
    pub port: u16,
    pub pairs: Vec<PairConfig>,
    pub spread_pct: f64,
    pub volatility: f64,
    pub fill_probability: f64,
    pub update_interval_ms: u64,
    pub starting_usd: f64,
    pub seed: Option<u64>,
    pub scenario: Option<ScenarioFile>,
}

#[derive(Debug, Clone)]
pub struct PairConfig {
    pub symbol: String,      // e.g. "OMG/USD"
    pub starting_price: f64, // e.g. 0.50
}

impl MockConfig {
    pub fn from_env() -> Self {
        let port = env_or("MOCK_PORT", "3050").parse().unwrap_or(3050);
        let spread_pct = env_or("MOCK_SPREAD_PCT", "5.0")
            .parse()
            .unwrap_or(5.0);
        let volatility = env_or("MOCK_VOLATILITY", "0.3")
            .parse()
            .unwrap_or(0.3);
        let fill_probability = env_or("MOCK_FILL_PROBABILITY", "0.3")
            .parse()
            .unwrap_or(0.3);
        let update_interval_ms = env_or("MOCK_UPDATE_INTERVAL_MS", "2000")
            .parse()
            .unwrap_or(2000);
        let starting_usd = env_or("MOCK_STARTING_USD", "10000")
            .parse()
            .unwrap_or(10000.0);
        let seed = std::env::var("MOCK_SEED")
            .ok()
            .and_then(|s| s.parse().ok());

        let pairs_str = env_or(
            "MOCK_PAIRS",
            "OMG/USD:0.50,CAMP/USD:0.004,SUP/USD:0.007",
        );
        let mut pairs: Vec<PairConfig> = pairs_str
            .split(',')
            .filter_map(|entry| {
                let parts: Vec<&str> = entry.trim().split(':').collect();
                if parts.len() == 2 {
                    let symbol = parts[0].trim().to_string();
                    let price = parts[1].trim().parse().ok()?;
                    Some(PairConfig {
                        symbol,
                        starting_price: price,
                    })
                } else {
                    None
                }
            })
            .collect();

        let scenario = std::env::var("MOCK_SCENARIO_FILE")
            .ok()
            .map(|path| {
                ScenarioFile::load(&path).expect("Failed to load scenario file")
            });

        // Merge scenario pairs into the pairs list
        if let Some(ref sc) = scenario {
            for (symbol, ps) in &sc.pairs {
                if let Some(existing) = pairs.iter_mut().find(|p| p.symbol == *symbol) {
                    existing.starting_price = ps.starting_price;
                } else {
                    pairs.push(PairConfig {
                        symbol: symbol.clone(),
                        starting_price: ps.starting_price,
                    });
                }
            }
        }

        MockConfig {
            port,
            pairs,
            spread_pct,
            volatility,
            fill_probability,
            update_interval_ms,
            starting_usd,
            seed,
            scenario,
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
