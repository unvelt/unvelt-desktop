//! Input intensity and the idle/active edges — active versus passive, and
//! content-free by construction.
//!
//! No keystroke is ever read. Each tick notes only whether ANY input happened
//! in the window (the idle timer reset), and once a minute emits an `input`
//! event carrying the fraction of ticks that were active. A ratio near 1.0 is
//! typing hard; a ratio near 0 while the foreground app is a video is passive
//! watching. That distinction is the entire point, and it needs no content.

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

pub struct ActivityHandler {
    interval: f64,
    window_ms: i64,
    /// Input counts as "happened" if the idle timer is below this. It is 1.5
    /// sample periods rather than one, because the poll and the input do not
    /// line up and a strict comparison drops roughly a third of real activity.
    active_gap: f64,
    win_s: u64,
    /// `None` between windows. Deliberately not a sentinel `0`: this field
    /// holds a wall-clock timestamp, and the Python original got away with
    /// using 0 as "unset" only because epoch-millis are never actually 0.
    /// That is a coincidence, not an invariant, and it made the first window
    /// after an epoch-zero clock silently never close.
    win_start: Option<i64>,
    active: u32,
    total: u32,
    away: bool,
}

impl ActivityHandler {
    pub fn new(cfg: &Config) -> Self {
        ActivityHandler {
            interval: cfg.sample_sec as f64,
            window_ms: cfg.input_window_sec as i64 * 1000,
            active_gap: cfg.sample_sec as f64 * 1.5,
            win_s: cfg.input_window_sec,
            win_start: None,
            active: 0,
            total: 0,
            away: false,
        }
    }
}

impl Handler for ActivityHandler {
    fn name(&self) -> &'static str {
        "activity"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let mut out = Vec::new();

        if tick.away && !self.away {
            self.away = true;
            out.push(tick.event(
                "desktop",
                "idle",
                tick.now,
                format!("dt:{}:idle:{}", tick.cfg.did, tick.now),
                Some(serde_json::json!({"after_s": tick.idle as i64})),
            ));
        } else if !tick.away && self.away {
            self.away = false;
            out.push(tick.event(
                "desktop",
                "active",
                tick.now,
                format!("dt:{}:active:{}", tick.cfg.did, tick.now),
                None,
            ));
        }

        if !tick.away {
            let start = *self.win_start.get_or_insert(tick.now);
            let _ = start;
            self.total += 1;
            if tick.idle < self.active_gap {
                self.active += 1;
            }
        }

        let closes = self
            .win_start
            .is_some_and(|s| (tick.now - s) >= self.window_ms && self.total > 0);
        if closes {
            let win_start = self.win_start.unwrap();
            // Two decimals, matching the Python `round(x, 2)`. The eid is keyed
            // on the window START, not on now, so a window recomputed after a
            // restart collides with itself instead of duplicating.
            let ratio = (self.active as f64 / self.total as f64 * 100.0).round() / 100.0;
            out.push(tick.event(
                "desktop",
                "input",
                tick.now,
                format!("dt:{}:input:{}", tick.cfg.did, win_start),
                Some(serde_json::json!({
                    "ratio": ratio,
                    "samples": self.total,
                    "win_s": self.win_s,
                })),
            ));
            self.win_start = None;
            self.active = 0;
            self.total = 0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, Front, Power};

    struct Fake {
        idle: f64,
    }
    impl Backend for Fake {
        fn frontmost(&self) -> Option<Front> {
            None
        }
        fn idle(&self) -> f64 {
            self.idle
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
            None
        }
    }

    fn drive(h: &mut ActivityHandler, cfg: &Config, samples: &[(i64, f64, bool)]) -> Vec<Event> {
        let mut out = Vec::new();
        for &(now, idle, away) in samples {
            let mut b = Fake { idle };
            let mut t = Tick {
                cfg,
                now,
                idle,
                away,
                backend: &mut b,
            };
            out.extend(h.poll(&mut t));
        }
        out
    }

    #[test]
    fn a_full_window_of_input_reports_ratio_one() {
        let cfg = Config::for_test();
        let mut h = ActivityHandler::new(&cfg);
        // 13 ticks of 5s: the window closes once 60s have elapsed.
        let samples: Vec<(i64, f64, bool)> = (0..13).map(|i| (i * 5_000, 0.5, false)).collect();
        let evs = drive(&mut h, &cfg, &samples);
        let input: Vec<_> = evs.iter().filter(|e| e.et == "input").collect();
        assert_eq!(input.len(), 1, "exactly one window closed");
        let p = input[0].p.as_ref().unwrap();
        assert_eq!(p["ratio"], 1.0);
        assert_eq!(p["win_s"], 60);
    }

    #[test]
    fn a_window_with_no_input_reports_ratio_zero_not_silence() {
        let cfg = Config::for_test();
        let mut h = ActivityHandler::new(&cfg);
        // Idle keeps climbing but stays under the away threshold: present at
        // the machine, not touching it. That is a real ratio of 0, and it is
        // the signal that separates watching from working.
        let samples: Vec<(i64, f64, bool)> = (0..13)
            .map(|i| (i * 5_000, 20.0 + i as f64, false))
            .collect();
        let evs = drive(&mut h, &cfg, &samples);
        let input: Vec<_> = evs.iter().filter(|e| e.et == "input").collect();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0].p.as_ref().unwrap()["ratio"], 0.0);
    }

    #[test]
    fn idle_and_active_edges_fire_once_each() {
        let cfg = Config::for_test();
        let mut h = ActivityHandler::new(&cfg);
        let evs = drive(
            &mut h,
            &cfg,
            &[
                (0, 1.0, false),
                (5_000, 200.0, true),
                (10_000, 205.0, true), // still away: no second idle event
                (15_000, 0.2, false),
                (20_000, 0.3, false), // still active: no second active event
            ],
        );
        let kinds: Vec<&str> = evs.iter().map(|e| e.et).collect();
        assert_eq!(kinds, vec!["idle", "active"]);
        assert_eq!(evs[0].p.as_ref().unwrap()["after_s"], 200);
    }

    #[test]
    fn away_ticks_do_not_count_as_samples() {
        let cfg = Config::for_test();
        let mut h = ActivityHandler::new(&cfg);
        // One sample of presence, then a long absence. The window still closes
        // on elapsed wall time -- that is what the Python collector does and
        // this is a parity port -- but it closes carrying `samples: 1`, not
        // twenty samples of zero. The distinction matters: `samples` is what
        // tells a reader the ratio rests on one observation rather than a full
        // minute of them, and inventing the other nineteen would turn an
        // absence into evidence of presence.
        let mut samples: Vec<(i64, f64, bool)> = vec![(0, 0.5, false)];
        samples.extend((1..20).map(|i| (i * 5_000, 300.0, true)));
        let evs = drive(&mut h, &cfg, &samples);
        let input: Vec<_> = evs.iter().filter(|e| e.et == "input").collect();
        assert_eq!(input.len(), 1);
        let p = input[0].p.as_ref().unwrap();
        assert_eq!(p["samples"], 1, "away ticks were counted as samples");
        assert_eq!(p["ratio"], 1.0);
    }
}
