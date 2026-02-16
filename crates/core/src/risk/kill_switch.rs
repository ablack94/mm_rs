use rust_decimal::Decimal;
use crate::config::RiskConfig;

/// Tracks whether the kill switch has been triggered.
#[derive(Debug, Clone, Default)]
pub struct KillSwitch {
    pub triggered: bool,
    pub reason: String,
}

impl KillSwitch {
    pub fn check(&mut self, realized_pnl: Decimal, config: &RiskConfig) -> bool {
        if self.triggered {
            return true;
        }
        if realized_pnl < config.kill_switch_loss_usd {
            self.triggered = true;
            self.reason = format!(
                "Realized P&L {} < threshold {}",
                realized_pnl, config.kill_switch_loss_usd
            );
            tracing::error!("KILL SWITCH: {}", self.reason);
            return true;
        }
        false
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
    fn test_not_triggered_initially() {
        let ks = KillSwitch::default();
        assert!(!ks.triggered);
    }

    #[test]
    fn test_triggers_on_loss() {
        let mut ks = KillSwitch::default();
        let config = test_risk_config();
        // realized_pnl of -150 is below the -100 threshold
        let result = ks.check(dec!(-150), &config);
        assert!(result);
        assert!(ks.triggered);
        assert!(!ks.reason.is_empty());
    }

    #[test]
    fn test_no_trigger_above_threshold() {
        let mut ks = KillSwitch::default();
        let config = test_risk_config();
        // realized_pnl of -50 is above the -100 threshold
        let result = ks.check(dec!(-50), &config);
        assert!(!result);
        assert!(!ks.triggered);
    }

    #[test]
    fn test_stays_triggered() {
        let mut ks = KillSwitch::default();
        let config = test_risk_config();
        // Trigger the kill switch
        ks.check(dec!(-150), &config);
        assert!(ks.triggered);
        // Even if pnl recovers, it stays triggered
        let result = ks.check(dec!(50), &config);
        assert!(result);
        assert!(ks.triggered);
    }
}
