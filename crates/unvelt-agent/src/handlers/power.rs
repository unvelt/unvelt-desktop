//! Battery level and AC state — mobile versus docked context.
//!
//! Emits on any AC change and on battery moves of 5 points or more. The
//! threshold is what keeps a trickle of one-percent updates from drowning the
//! signal: the interesting fact is "unplugged and draining", not each step.

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

pub struct PowerHandler {
    interval: f64,
    last: Option<(bool, Option<u8>)>,
}

impl PowerHandler {
    pub fn new(cfg: &Config) -> Self {
        PowerHandler {
            interval: cfg.context_sec as f64,
            last: None,
        }
    }
}

impl Handler for PowerHandler {
    fn name(&self) -> &'static str {
        "power"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let Some(pw) = tick.backend.power() else {
            return Vec::new();
        };
        if let Some((prev_ac, prev_pct)) = self.last {
            let moved = (prev_pct.unwrap_or(0) as i16 - pw.pct.unwrap_or(0) as i16).abs() >= 5;
            if prev_ac == pw.ac && !moved {
                return Vec::new();
            }
        }
        self.last = Some((pw.ac, pw.pct));
        vec![tick.event(
            "desktop",
            "power",
            tick.now,
            format!("dt:{}:power:{}", tick.cfg.did, tick.now),
            Some(serde_json::json!({
                "ac": if pw.ac { 1 } else { 0 },
                "pct": pw.pct,
            })),
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, Front, Power};

    struct Fake {
        power: Option<Power>,
    }
    impl Backend for Fake {
        fn frontmost(&self) -> Option<Front> {
            None
        }
        fn idle(&self) -> f64 {
            0.0
        }
        fn fullscreen(&self) -> Option<bool> {
            None
        }
        fn locked(&self) -> Option<bool> {
            None
        }
        fn ssid(&mut self) -> Option<String> {
            None
        }
        fn monitors(&mut self) -> Option<i32> {
            None
        }
        fn net(&mut self) -> Option<String> {
            None
        }
        fn power(&self) -> Option<Power> {
            self.power.as_ref().map(|p| Power {
                ac: p.ac,
                pct: p.pct,
            })
        }
    }

    fn drive(h: &mut PowerHandler, cfg: &Config, states: &[(bool, Option<u8>)]) -> Vec<String> {
        let mut out = Vec::new();
        for (i, &(ac, pct)) in states.iter().enumerate() {
            let mut b = Fake {
                power: Some(Power { ac, pct }),
            };
            let mut t = Tick {
                cfg,
                now: i as i64 * 1000,
                idle: 0.0,
                away: false,
                backend: &mut b,
            };
            for e in h.poll(&mut t) {
                out.push(e.p.unwrap().to_string());
            }
        }
        out
    }

    #[test]
    fn small_battery_moves_are_suppressed_but_ac_changes_never_are() {
        let cfg = Config::for_test();
        let mut h = PowerHandler::new(&cfg);
        let got = drive(
            &mut h,
            &cfg,
            &[
                (false, Some(80)), // first reading
                (false, Some(78)), // -2: below threshold
                (false, Some(75)), // -5 from 80: reported
                (true, Some(75)),  // plugged in: always reported
                (true, Some(76)),  // +1 on mains: suppressed
            ],
        );
        assert_eq!(
            got,
            vec![
                r#"{"ac":0,"pct":80}"#,
                r#"{"ac":0,"pct":75}"#,
                r#"{"ac":1,"pct":75}"#,
            ]
        );
    }

    #[test]
    fn a_machine_with_no_battery_reports_nothing_rather_than_zero() {
        let cfg = Config::for_test();
        let mut h = PowerHandler::new(&cfg);
        let mut b = Fake { power: None };
        let mut t = Tick {
            cfg: &cfg,
            now: 0,
            idle: 0.0,
            away: false,
            backend: &mut b,
        };
        assert!(h.poll(&mut t).is_empty());
    }
}
