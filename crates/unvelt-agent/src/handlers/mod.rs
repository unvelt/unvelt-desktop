//! Handler contract.
//!
//! A handler is a small stateful object that turns one kind of OS signal into
//! events. The controller calls `poll(tick)` no more often than `interval()`
//! seconds and spools whatever it returns. Handlers own their own change
//! detection and never touch the network or the spool directly.
//!
//! Ported one-for-one from `compound/handlers/`, including the `eid` formulas,
//! which are what make a retried batch dedupe instead of duplicating.

use crate::backend::Backend;
use crate::config::Config;
use crate::envelope::{self, Event};

/// The shared per-cycle snapshot handed to every handler.
///
/// `idle` and `away` are read once per cycle by the controller rather than by
/// each handler, because three handlers asking the OS the same question five
/// seconds apart would be three different answers to one question.
pub struct Tick<'a> {
    pub cfg: &'a Config,
    pub now: i64,
    pub idle: f64,
    pub away: bool,
    pub backend: &'a mut dyn Backend,
}

impl Tick<'_> {
    pub fn event(
        &self,
        src: &'static str,
        et: &'static str,
        ts: i64,
        eid: String,
        p: Option<serde_json::Value>,
    ) -> Event {
        envelope::build(self.cfg, src, et, ts, eid, p)
    }
}

pub trait Handler {
    fn name(&self) -> &'static str;
    fn interval(&self) -> f64;
    fn poll(&mut self, tick: &mut Tick) -> Vec<Event>;
}

mod activity;
mod focus;
mod location;
mod power;
mod session;

pub use activity::ActivityHandler;
pub use focus::FocusHandler;
pub use location::LocationHandler;
pub use power::PowerHandler;
pub use session::SessionHandler;

/// The default set, in poll order. Add a source by writing a Handler and
/// appending it here.
pub fn build_default(cfg: &Config) -> Vec<Box<dyn Handler>> {
    vec![
        Box::new(FocusHandler::new(cfg)),
        Box::new(ActivityHandler::new(cfg)),
        Box::new(SessionHandler::new(cfg)),
        Box::new(LocationHandler::new(cfg)),
        Box::new(PowerHandler::new(cfg)),
    ]
}
