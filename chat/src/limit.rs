//! The spam guard: a sliding-window limiter keyed by an arbitrary string.
//!
//! A direct port of SonicRoom's `server/src/chat-util.ts`, including the one
//! property that is easy to get wrong: **a blocked attempt does not count**.
//! Recording rejections would extend the window every time a flooder retried,
//! so a script hammering the endpoint would lock itself -- and anyone sharing
//! its key -- out permanently instead of merely being throttled. Here the
//! sender is free again exactly `window` after their last *accepted* message.
//!
//! Deterministic by construction: the caller passes `now`, so the whole thing
//! is testable without sleeping.

use std::collections::HashMap;

/// Accepted messages per window, per sender. SonicRoom's numbers, which have
/// been in front of screen-reader users in a live room: a flood is worse than
/// usual here because every message is *spoken*, and five in ten seconds is
/// already more than a listener can follow.
pub const RATE: usize = 5;

/// The window those five are counted over, in milliseconds.
pub const WINDOW_MS: u64 = 10_000;

/// The ceiling for one *address*, across everyone behind it.
///
/// The per-sender budget above is keyed on an identity a client can throw away
/// and regenerate, so on its own it stops an honest mistake and not a script.
/// This second, looser window is keyed on the address alone and is what a
/// flooder actually runs into. Four times the individual budget, so a household
/// or a shared office is not throttled for being several people.
pub const IP_RATE: usize = RATE * 4;

/// What the broadcaster gets instead. Higher because the host key is held by
/// one person who is also driving a broadcast: a now-playing announcement fired
/// by the app must never be dropped because the host happened to be typing.
/// Still bounded -- a leaked key should throttle, not have the run of the room.
pub const HOST_RATE: usize = 30;

#[derive(Debug)]
pub struct Limiter {
    limit: usize,
    window_ms: u64,
    hits: HashMap<String, Vec<u64>>,
}

/// The verdict, with the wait attached. Telling a client *when* to come back is
/// what lets its own composer disable itself for exactly that long instead of
/// guessing -- and what keeps a well-behaved script from retrying in a spin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Blocked { retry_after_ms: u64 },
}

impl Limiter {
    pub fn new(limit: usize, window_ms: u64) -> Self {
        Self {
            limit,
            window_ms,
            hits: HashMap::new(),
        }
    }

    /// Records a send and allows it, or refuses it and records nothing.
    pub fn check(&mut self, key: &str, now: u64) -> Verdict {
        let window = self.window_ms;
        let recent = self.hits.entry(key.to_string()).or_default();
        recent.retain(|t| now.saturating_sub(*t) < window);
        if recent.len() >= self.limit {
            // The oldest hit in the window is the one that has to age out
            // before there is room for another.
            let oldest = recent.first().copied().unwrap_or(now);
            return Verdict::Blocked {
                retry_after_ms: window.saturating_sub(now.saturating_sub(oldest)),
            };
        }
        recent.push(now);
        Verdict::Allowed
    }

    /// Drops every key whose window has fully aged out.
    ///
    /// Without this the map is a slow leak keyed by IP address, which on a
    /// public room is unbounded -- the one way a chat server with no database
    /// still manages to run out of memory.
    pub fn sweep(&mut self, now: u64) {
        let window = self.window_ms;
        self.hits
            .retain(|_, hits| hits.iter().any(|t| now.saturating_sub(*t) < window));
    }

    #[cfg(test)]
    pub fn tracked_keys(&self) -> usize {
        self.hits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_spent_and_then_recovers() {
        let mut limiter = Limiter::new(RATE, WINDOW_MS);
        for i in 0..RATE {
            assert_eq!(limiter.check("a", 1000 + i as u64), Verdict::Allowed);
        }
        assert!(matches!(
            limiter.check("a", 1005),
            Verdict::Blocked { .. }
        ));
        // The first hit was at 1000, so at 11_000 it has aged out and exactly
        // one slot is free again.
        assert_eq!(limiter.check("a", 11_000), Verdict::Allowed);
    }

    #[test]
    fn a_blocked_attempt_does_not_extend_the_window() {
        let mut limiter = Limiter::new(2, 1000);
        assert_eq!(limiter.check("a", 0), Verdict::Allowed);
        assert_eq!(limiter.check("a", 10), Verdict::Allowed);
        // Hammer it all the way through the window. If rejections counted, the
        // last of these would push the window out past 1010 and the sender
        // would still be blocked at 1010.
        for t in 20..1000 {
            assert!(matches!(limiter.check("a", t), Verdict::Blocked { .. }));
        }
        assert_eq!(limiter.check("a", 1000), Verdict::Allowed);
    }

    #[test]
    fn the_retry_hint_counts_down_to_the_oldest_hit_ageing_out() {
        let mut limiter = Limiter::new(1, 1000);
        assert_eq!(limiter.check("a", 0), Verdict::Allowed);
        assert_eq!(
            limiter.check("a", 250),
            Verdict::Blocked {
                retry_after_ms: 750
            }
        );
        assert_eq!(
            limiter.check("a", 900),
            Verdict::Blocked {
                retry_after_ms: 100
            }
        );
    }

    #[test]
    fn senders_do_not_share_a_budget() {
        let mut limiter = Limiter::new(1, 1000);
        assert_eq!(limiter.check("a", 0), Verdict::Allowed);
        assert_eq!(limiter.check("b", 0), Verdict::Allowed);
        assert!(matches!(limiter.check("a", 0), Verdict::Blocked { .. }));
    }

    #[test]
    fn sweeping_forgets_senders_who_have_gone_quiet() {
        let mut limiter = Limiter::new(RATE, WINDOW_MS);
        limiter.check("a", 0);
        limiter.check("b", 9_000);
        // Just short of the window: nobody has aged out yet, and sweeping early
        // must not forget a sender who still has hits that count against them.
        limiter.sweep(9_500);
        assert_eq!(limiter.tracked_keys(), 2);
        limiter.sweep(10_500); // "a" aged out, "b" is still live
        assert_eq!(limiter.tracked_keys(), 1);
        limiter.sweep(30_000);
        assert_eq!(limiter.tracked_keys(), 0);
    }
}
