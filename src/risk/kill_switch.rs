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
