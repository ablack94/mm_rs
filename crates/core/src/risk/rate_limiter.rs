use crate::config::RiskConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateStatus {
    Ok,
    Warn,
    Block,
}

/// Tracks the exchange rate limit counter.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    pub counter: f64,
    max: u32,
}

impl RateLimiter {
    pub fn new(config: &RiskConfig) -> Self {
        Self {
            counter: 0.0,
            max: config.rate_limit_max_counter,
        }
    }

    pub fn update(&mut self, counter: f64) {
        self.counter = counter;
    }

    pub fn status(&self) -> RateStatus {
        let ratio = self.counter / self.max as f64;
        if ratio >= 0.95 {
            RateStatus::Block
        } else if ratio >= 0.75 {
            RateStatus::Warn
        } else {
            RateStatus::Ok
        }
    }
}
