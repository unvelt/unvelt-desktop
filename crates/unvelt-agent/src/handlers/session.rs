//! Screen lock and unlock edges.
//!
//! Sleep and wake blind windows are handled by the controller itself, which
//! owns the loop clock and can see a wall-clock jump; this handler only reports
//! lock state where the backend can see it. It no-ops when `locked()` is None,
//! which is currently macOS and Linux.
//!
//! These two event types spent the whole POC unregistered: the Python collector
//! emitted them, ingest does not validate event types against the registry, so
//! they landed in `raw.events` and were read by nothing at all. They are in
//! `work.yaml` now (registry v17).

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

pub struct SessionHandler {
    interval: f64,
    locked: Option<bool>,
}

impl SessionHandler {
    pub fn new(cfg: &Config) -> Self {
        SessionHandler {
            interval: (cfg.sample_sec as f64).max(10.0),
            locked: None,
        }
    }
}

impl Handler for SessionHandler {
    fn name(&self) -> &'static str {
        "session"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let Some(lk) = tick.backend.locked() else {
            return Vec::new();
        };
        // First observation seeds the baseline without emitting. Starting the
        // agent while the screen happens to be unlocked is not an unlock, and
        // an event for it would put a phantom edge at every process start.
        let Some(prev) = self.locked else {
            self.locked = Some(lk);
            return Vec::new();
        };
        if lk == prev {
            return Vec::new();
        }
        self.locked = Some(lk);
        let et = if lk { "lock" } else { "unlock" };
        vec![tick.event(
            "desktop",
            et,
            tick.now,
            format!("dt:{}:{}:{}", tick.cfg.did, et, tick.now),
            None,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, Front, Power};

    struct Fake {
        locked: Option<bool>,
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
            self.locked
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

    fn drive(h: &mut SessionHandler, cfg: &Config, states: &[Option<bool>]) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (i, &lk) in states.iter().enumerate() {
            let mut b = Fake { locked: lk };
            let mut t = Tick {
                cfg,
                now: i as i64 * 1000,
                idle: 0.0,
                away: false,
                backend: &mut b,
            };
            out.extend(h.poll(&mut t).iter().map(|e| e.et));
        }
        out
    }

    #[test]
    fn only_transitions_emit_and_the_first_reading_seeds() {
        let cfg = Config::for_test();
        let mut h = SessionHandler::new(&cfg);
        let got = drive(
            &mut h,
            &cfg,
            &[
                Some(false), // baseline, silent
                Some(false),
                Some(true), // lock
                Some(true),
                Some(false), // unlock
            ],
        );
        assert_eq!(got, vec!["lock", "unlock"]);
    }

    #[test]
    fn a_platform_that_cannot_answer_stays_silent() {
        let cfg = Config::for_test();
        let mut h = SessionHandler::new(&cfg);
        assert!(drive(&mut h, &cfg, &[None, None, None]).is_empty());
    }
}
