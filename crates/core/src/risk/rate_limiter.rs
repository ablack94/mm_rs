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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn test_risk_config() -> RiskConfig {
        RiskConfig {
            max_inventory_usd: dec!(200),
            max_total_exposure_usd: dec!(2000),
            kill_switch_loss_usd: dec!(-100),
            stale_order_secs: 300,
            dms_timeout_secs: 60,
            dms_refresh_secs: 20,
            rate_limit_max_counter: 60,
            stop_loss_pct: dec!(0.03),
            take_profit_pct: dec!(0.10),
            cooldown_after_liquidation_secs: 3600,
        }
    }

    #[test]
    fn test_initial_status_ok() {
        let config = test_risk_config();
        let rl = RateLimiter::new(&config);
        assert_eq!(rl.status(), RateStatus::Ok);
    }

    #[test]
    fn test_warn_at_75_pct() {
        let config = test_risk_config();
        let mut rl = RateLimiter::new(&config);
        // 75% of 60 = 45
        rl.update(45.0);
        assert_eq!(rl.status(), RateStatus::Warn);
    }

    #[test]
    fn test_block_at_95_pct() {
        let config = test_risk_config();
        let mut rl = RateLimiter::new(&config);
        // 95% of 60 = 57
        rl.update(57.0);
        assert_eq!(rl.status(), RateStatus::Block);
    }

    #[test]
    fn test_update_changes_status() {
        let config = test_risk_config();
        let mut rl = RateLimiter::new(&config);
        assert_eq!(rl.status(), RateStatus::Ok);
        // Update to 95% of 60 = 57
        rl.update(57.0);
        assert_eq!(rl.status(), RateStatus::Block);
    }

    #[test]
    fn test_below_75_is_ok() {
        let config = test_risk_config();
        let mut rl = RateLimiter::new(&config);
        // 74% of 60 = 44.4
        rl.update(44.4);
        assert_eq!(rl.status(), RateStatus::Ok);
    }
}
