use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioFile {
    pub name: String,
    pub description: Option<String>,
    pub defaults: Option<ScenarioDefaults>,
    pub pairs: HashMap<String, PairScenario>,
    pub end_behavior: Option<EndBehavior>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ScenarioDefaults {
    pub spread_pct: Option<f64>,
    pub fill_probability: Option<f64>,
    pub interpolation: Option<Interpolation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairScenario {
    pub starting_price: f64,
    pub waypoints: Vec<Waypoint>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Waypoint {
    pub t_ms: u64,
    pub mid: f64,
    pub spread_pct: Option<f64>,
    pub fill_probability: Option<f64>,
    pub interpolation: Option<Interpolation>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Interpolation {
    #[default]
    Linear,
    Step,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EndBehavior {
    #[default]
    HoldLast,
    Loop,
    Stop,
}

/// Result of sampling a scenario at a given time.
pub struct ScenarioSample {
    pub mid: f64,
    pub spread_pct: Option<f64>,
    pub fill_probability: Option<f64>,
    pub finished: bool,
}

impl ScenarioFile {
    pub fn load(path: &str) -> Result<Self, anyhow::Error> {
        let contents = std::fs::read_to_string(path)?;
        let scenario: ScenarioFile = serde_json::from_str(&contents)?;
        scenario.validate()?;
        Ok(scenario)
    }

    fn validate(&self) -> Result<(), anyhow::Error> {
        for (symbol, ps) in &self.pairs {
            anyhow::ensure!(
                ps.waypoints.len() >= 2,
                "{symbol}: need at least 2 waypoints, got {}",
                ps.waypoints.len()
            );
            anyhow::ensure!(
                ps.waypoints[0].t_ms == 0,
                "{symbol}: first waypoint must have t_ms=0, got {}",
                ps.waypoints[0].t_ms
            );
            for w in ps.waypoints.windows(2) {
                anyhow::ensure!(
                    w[1].t_ms > w[0].t_ms,
                    "{symbol}: waypoints must be strictly monotonically increasing \
                     (t_ms {} followed by {})",
                    w[0].t_ms,
                    w[1].t_ms
                );
            }
            anyhow::ensure!(
                (ps.starting_price - ps.waypoints[0].mid).abs() < 1e-10,
                "{symbol}: starting_price ({}) must match first waypoint mid ({})",
                ps.starting_price,
                ps.waypoints[0].mid
            );
            for wp in &ps.waypoints {
                anyhow::ensure!(
                    wp.mid > 0.0,
                    "{symbol}: all mid values must be positive, got {} at t_ms={}",
                    wp.mid,
                    wp.t_ms
                );
                if let Some(sp) = wp.spread_pct {
                    anyhow::ensure!(
                        sp > 0.0,
                        "{symbol}: spread_pct must be positive, got {} at t_ms={}",
                        sp,
                        wp.t_ms
                    );
                }
                if let Some(fp) = wp.fill_probability {
                    anyhow::ensure!(
                        (0.0..=1.0).contains(&fp),
                        "{symbol}: fill_probability must be in [0, 1], got {} at t_ms={}",
                        fp,
                        wp.t_ms
                    );
                }
            }
        }
        Ok(())
    }
}

impl PairScenario {
    /// Given elapsed milliseconds since scenario start, compute the current
    /// mid price and any per-segment overrides.
    pub fn sample_at(
        &self,
        elapsed_ms: u64,
        defaults: &ScenarioDefaults,
        end_behavior: EndBehavior,
    ) -> ScenarioSample {
        let wps = &self.waypoints;
        let last = &wps[wps.len() - 1];

        // Past the end of the scenario
        if elapsed_ms >= last.t_ms {
            match end_behavior {
                EndBehavior::HoldLast => {
                    return ScenarioSample {
                        mid: last.mid,
                        spread_pct: last.spread_pct.or(defaults.spread_pct),
                        fill_probability: last.fill_probability.or(defaults.fill_probability),
                        finished: false,
                    };
                }
                EndBehavior::Stop => {
                    return ScenarioSample {
                        mid: last.mid,
                        spread_pct: last.spread_pct.or(defaults.spread_pct),
                        fill_probability: last.fill_probability.or(defaults.fill_probability),
                        finished: true,
                    };
                }
                EndBehavior::Loop => {
                    let total_duration = last.t_ms;
                    if total_duration == 0 {
                        return ScenarioSample {
                            mid: last.mid,
                            spread_pct: last.spread_pct.or(defaults.spread_pct),
                            fill_probability: last.fill_probability.or(defaults.fill_probability),
                            finished: false,
                        };
                    }
                    let looped_ms = elapsed_ms % total_duration;
                    return self.sample_at(looped_ms, defaults, EndBehavior::HoldLast);
                }
            }
        }

        // Find the active segment
        let mut idx = 0;
        for i in 0..wps.len() - 1 {
            if elapsed_ms >= wps[i].t_ms && elapsed_ms < wps[i + 1].t_ms {
                idx = i;
                break;
            }
        }

        let w0 = &wps[idx];
        let w1 = &wps[idx + 1];

        let interp = w0
            .interpolation
            .or(defaults.interpolation)
            .unwrap_or_default();

        let mid = match interp {
            Interpolation::Linear => {
                let frac = (elapsed_ms - w0.t_ms) as f64 / (w1.t_ms - w0.t_ms) as f64;
                w0.mid + (w1.mid - w0.mid) * frac
            }
            Interpolation::Step => w0.mid,
        };

        ScenarioSample {
            mid,
            spread_pct: w0.spread_pct.or(defaults.spread_pct),
            fill_probability: w0.fill_probability.or(defaults.fill_probability),
            finished: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_scenario_json() -> String {
        r#"{
            "name": "test_scenario",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 0,     "mid": 0.50 },
                        { "t_ms": 10000, "mid": 0.60 }
                    ]
                }
            }
        }"#
        .to_string()
    }

    fn parse_scenario(json: &str) -> Result<ScenarioFile, anyhow::Error> {
        let scenario: ScenarioFile = serde_json::from_str(json)?;
        scenario.validate()?;
        Ok(scenario)
    }

    #[test]
    fn test_load_valid_scenario() {
        let sc = parse_scenario(&minimal_scenario_json()).unwrap();
        assert_eq!(sc.name, "test_scenario");
        assert!(sc.pairs.contains_key("OMG/USD"));
        let pair = &sc.pairs["OMG/USD"];
        assert_eq!(pair.starting_price, 0.50);
        assert_eq!(pair.waypoints.len(), 2);
        assert_eq!(pair.waypoints[0].t_ms, 0);
        assert_eq!(pair.waypoints[1].t_ms, 10000);
    }

    #[test]
    fn test_validate_rejects_fewer_than_2_waypoints() {
        let json = r#"{
            "name": "bad",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [{ "t_ms": 0, "mid": 0.50 }]
                }
            }
        }"#;
        let err = parse_scenario(json).unwrap_err();
        assert!(err.to_string().contains("at least 2 waypoints"));
    }

    #[test]
    fn test_validate_rejects_nonzero_first_t_ms() {
        let json = r#"{
            "name": "bad",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 100, "mid": 0.50 },
                        { "t_ms": 200, "mid": 0.60 }
                    ]
                }
            }
        }"#;
        let err = parse_scenario(json).unwrap_err();
        assert!(err.to_string().contains("t_ms=0"));
    }

    #[test]
    fn test_validate_rejects_non_monotonic() {
        let json = r#"{
            "name": "bad",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 0,     "mid": 0.50 },
                        { "t_ms": 1000,  "mid": 0.55 },
                        { "t_ms": 500,   "mid": 0.60 }
                    ]
                }
            }
        }"#;
        let err = parse_scenario(json).unwrap_err();
        assert!(err.to_string().contains("monotonically increasing"));
    }

    #[test]
    fn test_validate_rejects_starting_price_mismatch() {
        let json = r#"{
            "name": "bad",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 0,     "mid": 0.60 },
                        { "t_ms": 10000, "mid": 0.70 }
                    ]
                }
            }
        }"#;
        let err = parse_scenario(json).unwrap_err();
        assert!(err.to_string().contains("starting_price"));
    }

    #[test]
    fn test_sample_at_exact_waypoint() {
        let sc = parse_scenario(&minimal_scenario_json()).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        let s0 = pair.sample_at(0, &defaults, EndBehavior::HoldLast);
        assert!((s0.mid - 0.50).abs() < 1e-10);

        let s1 = pair.sample_at(10000, &defaults, EndBehavior::HoldLast);
        assert!((s1.mid - 0.60).abs() < 1e-10);
    }

    #[test]
    fn test_sample_at_linear_midpoint() {
        let sc = parse_scenario(&minimal_scenario_json()).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        // At t=5000 (halfway), should be 0.55
        let s = pair.sample_at(5000, &defaults, EndBehavior::HoldLast);
        assert!(
            (s.mid - 0.55).abs() < 1e-10,
            "expected 0.55, got {}",
            s.mid
        );
    }

    #[test]
    fn test_sample_at_step_holds_previous() {
        let json = r#"{
            "name": "step_test",
            "defaults": { "interpolation": "step" },
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 0,     "mid": 0.50 },
                        { "t_ms": 10000, "mid": 0.60 }
                    ]
                }
            }
        }"#;
        let sc = parse_scenario(json).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = sc.defaults.clone().unwrap_or_default();

        // At t=5000 with step interpolation, should still be 0.50
        let s = pair.sample_at(5000, &defaults, EndBehavior::HoldLast);
        assert!(
            (s.mid - 0.50).abs() < 1e-10,
            "expected 0.50, got {}",
            s.mid
        );

        // At t=9999, still 0.50
        let s = pair.sample_at(9999, &defaults, EndBehavior::HoldLast);
        assert!(
            (s.mid - 0.50).abs() < 1e-10,
            "expected 0.50, got {}",
            s.mid
        );
    }

    #[test]
    fn test_sample_at_hold_last() {
        let sc = parse_scenario(&minimal_scenario_json()).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        let s = pair.sample_at(99999, &defaults, EndBehavior::HoldLast);
        assert!((s.mid - 0.60).abs() < 1e-10);
        assert!(!s.finished);
    }

    #[test]
    fn test_sample_at_loop() {
        let sc = parse_scenario(&minimal_scenario_json()).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        // t=15000 with loop (period=10000) → looped_ms=5000 → mid=0.55
        let s = pair.sample_at(15000, &defaults, EndBehavior::Loop);
        assert!(
            (s.mid - 0.55).abs() < 1e-10,
            "expected 0.55, got {}",
            s.mid
        );
    }

    #[test]
    fn test_sample_at_stop() {
        let sc = parse_scenario(&minimal_scenario_json()).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        let s = pair.sample_at(99999, &defaults, EndBehavior::Stop);
        assert!((s.mid - 0.60).abs() < 1e-10);
        assert!(s.finished);
    }

    #[test]
    fn test_spread_override_per_segment() {
        let json = r#"{
            "name": "spread_test",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 0,     "mid": 0.50, "spread_pct": 5.0 },
                        { "t_ms": 10000, "mid": 0.50, "spread_pct": 15.0 },
                        { "t_ms": 20000, "mid": 0.50 }
                    ]
                }
            }
        }"#;
        let sc = parse_scenario(json).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        // In first segment: spread=5.0
        let s0 = pair.sample_at(5000, &defaults, EndBehavior::HoldLast);
        assert_eq!(s0.spread_pct, Some(5.0));

        // In second segment: spread=15.0
        let s1 = pair.sample_at(15000, &defaults, EndBehavior::HoldLast);
        assert_eq!(s1.spread_pct, Some(15.0));
    }

    #[test]
    fn test_fill_probability_override() {
        let json = r#"{
            "name": "fp_test",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 0,     "mid": 0.50, "fill_probability": 0.8 },
                        { "t_ms": 10000, "mid": 0.50, "fill_probability": 0.05 },
                        { "t_ms": 20000, "mid": 0.50 }
                    ]
                }
            }
        }"#;
        let sc = parse_scenario(json).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        let s0 = pair.sample_at(5000, &defaults, EndBehavior::HoldLast);
        assert_eq!(s0.fill_probability, Some(0.8));

        let s1 = pair.sample_at(15000, &defaults, EndBehavior::HoldLast);
        assert_eq!(s1.fill_probability, Some(0.05));
    }

    #[test]
    fn test_defaults_used_as_fallback() {
        let sc = parse_scenario(&minimal_scenario_json()).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults {
            spread_pct: Some(8.0),
            fill_probability: Some(0.5),
            interpolation: None,
        };

        let s = pair.sample_at(5000, &defaults, EndBehavior::HoldLast);
        assert_eq!(s.spread_pct, Some(8.0));
        assert_eq!(s.fill_probability, Some(0.5));
    }

    #[test]
    fn test_multi_segment_interpolation() {
        let json = r#"{
            "name": "multi",
            "pairs": {
                "OMG/USD": {
                    "starting_price": 0.50,
                    "waypoints": [
                        { "t_ms": 0,     "mid": 0.50 },
                        { "t_ms": 5000,  "mid": 0.50 },
                        { "t_ms": 8000,  "mid": 0.30 },
                        { "t_ms": 20000, "mid": 0.48 }
                    ]
                }
            }
        }"#;
        let sc = parse_scenario(json).unwrap();
        let pair = &sc.pairs["OMG/USD"];
        let defaults = ScenarioDefaults::default();

        // t=0: flat at 0.50
        let s = pair.sample_at(0, &defaults, EndBehavior::HoldLast);
        assert!((s.mid - 0.50).abs() < 1e-10);

        // t=3000: still flat at 0.50 (first segment is flat)
        let s = pair.sample_at(3000, &defaults, EndBehavior::HoldLast);
        assert!((s.mid - 0.50).abs() < 1e-10);

        // t=6500: midway through crash (0.50→0.30 over 3000ms, at 1500ms)
        // frac = 1500/3000 = 0.5, mid = 0.50 + (0.30 - 0.50)*0.5 = 0.40
        let s = pair.sample_at(6500, &defaults, EndBehavior::HoldLast);
        assert!(
            (s.mid - 0.40).abs() < 1e-10,
            "expected 0.40, got {}",
            s.mid
        );

        // t=8000: at bottom 0.30
        let s = pair.sample_at(8000, &defaults, EndBehavior::HoldLast);
        assert!((s.mid - 0.30).abs() < 1e-10);

        // t=14000: midway through recovery (0.30→0.48 over 12000ms, at 6000ms)
        // frac = 6000/12000 = 0.5, mid = 0.30 + 0.18*0.5 = 0.39
        let s = pair.sample_at(14000, &defaults, EndBehavior::HoldLast);
        assert!(
            (s.mid - 0.39).abs() < 1e-10,
            "expected 0.39, got {}",
            s.mid
        );
    }
}
