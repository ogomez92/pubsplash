//! Rooms: a history ring, a fan-out channel, and a listener count. No database.
//!
//! A room exists because somebody opened it and stops existing when nobody has
//! for a while ([`Rooms::sweep`]). That is the whole lifecycle -- there is no
//! create step, no owner record and nothing on disk -- which is what makes the
//! server a single file you copy to a box. The cost is that history does not
//! survive a restart, which is the right trade for live chat attached to a live
//! broadcast: the stream did not survive it either.
//!
//! Fan-out is one `tokio::sync::broadcast` per room. A slow listener that falls
//! [`CHANNEL_CAPACITY`] messages behind is *lagged* rather than disconnected --
//! the SSE handler reports the gap and carries on, because a listener on a
//! phone that slept for a minute should come back to a live room, not a dead
//! connection.

use crate::limit::Limiter;
use crate::message::{ANONYMOUS, Message, Reject, Stream, now_ms};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Messages kept per room and replayed to whoever opens the page next.
/// SonicRoom's number: enough that a listener joining mid-show can catch up,
/// short enough that arrowing through it is still a list and not an archive.
pub const HISTORY_MAX: usize = 100;

/// How far behind a listener may fall before the server stops keeping their
/// backlog. Generous: 256 messages at the accepted rate is minutes of chat.
const CHANNEL_CAPACITY: usize = 256;

/// How long a room with no listeners survives before it is forgotten.
const IDLE_TTL_MS: u64 = 60 * 60 * 1000;

/// How long a sender must keep a nickname before they may change it.
///
/// This is the whole anti-impersonation rule, and it is a cooldown rather than
/// a lock because the failure it prevents is *churn*: the nickname is a
/// free-text field on every message, so without this one person can answer
/// themselves as three people, or take the name of whoever spoke last, message
/// by message. Five minutes is long enough that a name means something within a
/// conversation, and short enough to fix a typo before the show is over.
const NAME_COOLDOWN_MS: u64 = 5 * 60 * 1000;

/// How long a claimed nickname is held after its owner's last message. Past
/// this it is free again, so a room does not accumulate names nobody is using.
const NAME_TTL_MS: u64 = 30 * 60 * 1000;

/// One sender's hold on a nickname in one room.
#[derive(Debug, Clone)]
struct Claim {
    nick: String,
    /// When this *name* was taken, which is what the cooldown is measured from.
    /// Deliberately not refreshed by sending: refreshing it would restart the
    /// cooldown on every message, and the name could then never be changed.
    claimed_at: u64,
    /// When the owner last spoke, which is what the TTL is measured from.
    last_seen: u64,
}

/// Who is speaking under which name in one room.
///
/// Two maps rather than one, so both questions are answered in constant time:
/// "what is this sender called" (the cooldown) and "who owns this name" (the
/// impersonation check).
#[derive(Debug, Default)]
struct Names {
    by_sender: HashMap<String, Claim>,
    /// Lowercased nickname to sender key. Lowercased because `Alice` and
    /// `alice` are the same name to everyone reading the room, and telling them
    /// apart by case is exactly how an impersonator would dress one up.
    by_nick: HashMap<String, String>,
}

impl Names {
    /// Decides what this sender is called, or refuses the message.
    ///
    /// `host` is proof of the room key, so the broadcaster is exempt from the
    /// cooldown and may *seize* a name a listener has taken. Without that, a
    /// troll who joined first and claimed the host's name would lock them out of
    /// their own room -- which is the very failure this rule exists to prevent.
    fn resolve(
        &mut self,
        sender: &str,
        requested: &str,
        host: bool,
        now: u64,
    ) -> Result<String, Reject> {
        // The anonymous default belongs to nobody: everyone who has not set a
        // name shares it, so claiming it would let the first of them lock out
        // all the rest. It neither takes a name nor gives one up -- releasing
        // the claim here was a hole straight through the cooldown, since one
        // unnamed message then bought an immediate rename.
        if requested == ANONYMOUS {
            if let Some(claim) = self.by_sender.get_mut(sender) {
                claim.last_seen = now;
            }
            return Ok(requested.to_string());
        }

        let key = requested.to_lowercase();
        if let Some(owner) = self.by_nick.get(&key) {
            if owner != sender && !host {
                return Err(Reject::NameTaken);
            }
        }

        match self.by_sender.get(sender) {
            // The same name as last time: nothing to decide.
            Some(claim) if claim.nick.to_lowercase() == key => {
                let claimed_at = claim.claimed_at;
                self.bind(sender, requested, claimed_at, now);
                Ok(requested.to_string())
            }
            // A change. Allowed once the cooldown has run; always for the host.
            Some(claim) => {
                if !host && now.saturating_sub(claim.claimed_at) < NAME_COOLDOWN_MS {
                    return Err(Reject::NameLocked);
                }
                let previous = claim.nick.to_lowercase();
                self.by_nick.remove(&previous);
                self.bind(sender, requested, now, now);
                Ok(requested.to_string())
            }
            None => {
                self.bind(sender, requested, now, now);
                Ok(requested.to_string())
            }
        }
    }

    fn bind(&mut self, sender: &str, nick: &str, claimed_at: u64, now: u64) {
        // A seizing host may be taking this name off somebody else, so the
        // previous owner's own entry goes with it: leaving it behind would have
        // two senders each believing they hold the one name.
        if let Some(previous) = self.by_nick.insert(nick.to_lowercase(), sender.to_string()) {
            if previous != sender {
                self.by_sender.remove(&previous);
            }
        }
        self.by_sender.insert(
            sender.to_string(),
            Claim {
                nick: nick.to_string(),
                claimed_at,
                last_seen: now,
            },
        );
    }

    /// How long until this sender may rename. Zero when they already may.
    fn cooldown_left(&self, sender: &str, now: u64) -> u64 {
        self.by_sender
            .get(sender)
            .map(|claim| NAME_COOLDOWN_MS.saturating_sub(now.saturating_sub(claim.claimed_at)))
            .unwrap_or(0)
    }

    fn sweep(&mut self, now: u64) {
        self.by_sender
            .retain(|_, claim| now.saturating_sub(claim.last_seen) < NAME_TTL_MS);
        let live: std::collections::HashSet<String> = self
            .by_sender
            .values()
            .map(|claim| claim.nick.to_lowercase())
            .collect();
        self.by_nick.retain(|nick, _| live.contains(nick));
    }
}

/// What goes out on the feed. Every variant is an SSE `event:` name, so adding
/// one is a protocol change both clients see (and both ignore politely if they
/// do not know it).
#[derive(Debug, Clone)]
pub enum Feed {
    Chat(Message),
    /// Where the show can be heard, or `None` when it goes off air. Sent when
    /// it changes, so a page that was already open gains its play button the
    /// moment the broadcast starts rather than on the next reload.
    Stream(Option<Stream>),
    /// How many feeds are open on this room. The page shows it; Pubsplash
    /// deliberately ignores it, because it counts real stream listeners from
    /// Icecast and a second, worse answer to the same question is not wanted.
    Listeners(usize),
}

pub struct Room {
    /// Newest last. A `Vec` rather than a `VecDeque` because it is read whole
    /// (replayed to a joiner) far more often than it is trimmed.
    history: Vec<Message>,
    tx: broadcast::Sender<Feed>,
    listeners: Arc<AtomicUsize>,
    /// When this room last had a listener or a message, epoch ms. Read by the
    /// sweeper to decide whether anyone still cares about it.
    last_active: Arc<AtomicU64>,
    /// Counter behind the per-message id, so two messages accepted in the same
    /// millisecond are still distinguishable -- which matters because the id is
    /// what both clients de-duplicate on.
    seq: u64,
    /// Who is speaking under which name here. See [`Names`].
    names: Names,
    /// Where this room's audience can hear the show, when the broadcaster has
    /// said. Not persisted and not inferred -- see [`Stream`].
    stream: Option<Stream>,
}

impl Room {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            history: Vec::new(),
            tx,
            listeners: Arc::new(AtomicUsize::new(0)),
            last_active: Arc::new(AtomicU64::new(now_ms())),
            seq: 0,
            names: Names::default(),
            stream: None,
        }
    }
}

/// A listener's handle on a room. Dropping it decrements the count and
/// announces the new one, so a closed tab is reflected without a heartbeat --
/// which is the whole reason this is a guard type rather than a pair of calls.
pub struct Listener {
    listeners: Arc<AtomicUsize>,
    last_active: Arc<AtomicU64>,
    tx: broadcast::Sender<Feed>,
    pub rx: broadcast::Receiver<Feed>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let left = self.listeners.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
        self.last_active.store(now_ms(), Ordering::Relaxed);
        // Send failure here is the normal end of the last listener leaving:
        // there is nobody to tell, which is exactly what the count would say.
        let _ = self.tx.send(Feed::Listeners(left));
    }
}

/// Everything the server keeps, which is one lock over a map of rooms plus the
/// two limiters.
///
/// One `Mutex` rather than a lock per room: every operation here is a few
/// microseconds of `Vec` work with no I/O under it, so the contention that
/// would justify finer locking cannot build up. Nothing in this module awaits
/// while holding it -- that is the rule that keeps the claim true.
pub struct Rooms {
    inner: Mutex<Inner>,
}

struct Inner {
    rooms: HashMap<String, Room>,
    limiter: Limiter,
    host_limiter: Limiter,
    /// The ceiling for a whole address. See [`crate::limit::IP_RATE`].
    address_limiter: Limiter,
}

impl Rooms {
    pub fn new(limiter: Limiter, host_limiter: Limiter, address_limiter: Limiter) -> Self {
        Self {
            inner: Mutex::new(Inner {
                rooms: HashMap::new(),
                limiter,
                host_limiter,
                address_limiter,
            }),
        }
    }

    /// Publishes (or withdraws) where this room's show can be heard.
    ///
    /// Announced to everybody already in the room as well as stored, so a
    /// listener who opened the page before the show started does not have to
    /// reload to get a play button.
    pub fn set_stream(&self, room: &str, stream: Option<Stream>) {
        let mut inner = self.lock();
        let entry = inner.rooms.entry(room.to_string()).or_insert_with(Room::new);
        if entry.stream == stream {
            return; // nothing changed; do not wake every listener to say so
        }
        entry.stream = stream.clone();
        entry.last_active.store(now_ms(), Ordering::Relaxed);
        let _ = entry.tx.send(Feed::Stream(stream));
    }

    /// Subscribes to a room, creating it if this is the first person through
    /// the door. Returns the guard, the history to replay, the stream if the
    /// broadcaster has published one, and the new count.
    pub fn join(&self, room: &str) -> (Listener, Vec<Message>, Option<Stream>, usize) {
        let mut inner = self.lock();
        let entry = inner.rooms.entry(room.to_string()).or_insert_with(Room::new);
        let count = entry.listeners.fetch_add(1, Ordering::SeqCst) + 1;
        entry.last_active.store(now_ms(), Ordering::Relaxed);
        let listener = Listener {
            listeners: entry.listeners.clone(),
            last_active: entry.last_active.clone(),
            tx: entry.tx.clone(),
            rx: entry.tx.subscribe(),
        };
        let history = entry.history.clone();
        let stream = entry.stream.clone();
        // Sent after subscribing, so the joiner's own arrival is the first
        // thing on their feed rather than a count they are already missing from.
        let _ = entry.tx.send(Feed::Listeners(count));
        (listener, history, stream, count)
    }

    /// Records a message and fans it out. Returns it with its assigned id.
    pub fn publish(&self, room: &str, mut message: Message) -> Message {
        let mut inner = self.lock();
        let entry = inner.rooms.entry(room.to_string()).or_insert_with(Room::new);
        entry.seq = entry.seq.wrapping_add(1);
        message.id = format!("{}-{}", message.ts, entry.seq);
        entry.last_active.store(now_ms(), Ordering::Relaxed);
        entry.history.push(message.clone());
        if entry.history.len() > HISTORY_MAX {
            let excess = entry.history.len() - HISTORY_MAX;
            entry.history.drain(..excess);
        }
        let _ = entry.tx.send(Feed::Chat(message.clone()));
        message
    }

    /// Asks the limiters whether this sender may speak.
    ///
    /// Two budgets, and both must allow it. `identity` is the individual, who
    /// gets the ordinary five-in-ten; `address` is everybody behind one IP, who
    /// share a looser ceiling. The second exists because an identity carries a
    /// token the client generates and can therefore throw away -- so on its own
    /// the first budget stops a fast typist and not a script.
    ///
    /// Neither budget is per room: somebody who floods one room does not get a
    /// fresh five messages by opening another, which is the first thing anyone
    /// tries.
    ///
    /// The address ceiling is checked FIRST and the individual one second, so a
    /// sender blocked by the address never has a hit recorded against their own
    /// budget as well -- being throttled for a neighbour's flood should not also
    /// cost them their own allowance.
    pub fn check_rate(
        &self,
        identity: &str,
        address: &str,
        host: bool,
        now: u64,
    ) -> crate::limit::Verdict {
        let mut inner = self.lock();
        // The host holds the room key, so they are not one of the crowd behind
        // an address and are not counted against it.
        if host {
            return inner.host_limiter.check(identity, now);
        }
        match inner.address_limiter.check(address, now) {
            crate::limit::Verdict::Allowed => inner.limiter.check(identity, now),
            blocked => blocked,
        }
    }

    /// Decides what `sender` is called in `room`, or refuses the message.
    ///
    /// Asked *before* the rate limiter, deliberately: a refused name must not
    /// cost the sender a message out of their budget, because the fix is to
    /// send again under the name they already have.
    ///
    /// The error carries how long is left on the cooldown, so the client can
    /// say when rather than only no.
    pub fn resolve_nick(
        &self,
        room: &str,
        sender: &str,
        requested: &str,
        host: bool,
        now: u64,
    ) -> Result<String, (Reject, u64)> {
        let mut inner = self.lock();
        let entry = inner.rooms.entry(room.to_string()).or_insert_with(Room::new);
        match entry.names.resolve(sender, requested, host, now) {
            Ok(nick) => Ok(nick),
            Err(why) => Err((why, entry.names.cooldown_left(sender, now))),
        }
    }

    /// Drops rooms nobody has used for [`IDLE_TTL_MS`], and forgets senders
    /// whose rate-limit windows have aged out.
    pub fn sweep(&self, now: u64) -> usize {
        let mut inner = self.lock();
        inner.rooms.retain(|_, room| {
            room.listeners.load(Ordering::SeqCst) > 0
                || now.saturating_sub(room.last_active.load(Ordering::Relaxed)) < IDLE_TTL_MS
        });
        for room in inner.rooms.values_mut() {
            room.names.sweep(now);
        }
        inner.limiter.sweep(now);
        inner.host_limiter.sweep(now);
        inner.address_limiter.sweep(now);
        inner.rooms.len()
    }

    /// `(rooms, listeners)`, for the health endpoint.
    pub fn census(&self) -> (usize, usize) {
        let inner = self.lock();
        let listeners = inner
            .rooms
            .values()
            .map(|r| r.listeners.load(Ordering::SeqCst))
            .sum();
        (inner.rooms.len(), listeners)
    }

    /// Recovers from a poisoned lock rather than propagating the panic.
    ///
    /// Nothing in here awaits while holding the lock and every operation is
    /// total, so the only way to poison it is a panic in an unrelated
    /// allocation -- at which point carrying on with the (structurally intact)
    /// map keeps every other room alive, and refusing to would take the whole
    /// server down over one of them.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit::{Limiter, RATE, WINDOW_MS};
    use crate::message::Message;

    fn rooms() -> Rooms {
        Rooms::new(
            Limiter::new(RATE, WINDOW_MS),
            Limiter::new(RATE * 6, WINDOW_MS),
            Limiter::new(RATE * 4, WINDOW_MS),
        )
    }

    fn message(text: &str) -> Message {
        Message {
            id: String::new(),
            nick: "Alice".into(),
            text: text.into(),
            ts: now_ms(),
            host: false,
            system: false,
        }
    }

    #[test]
    fn history_is_replayed_to_whoever_joins_next() {
        let rooms = rooms();
        rooms.publish("show", message("first"));
        rooms.publish("show", message("second"));
        let (_listener, history, _, count) = rooms.join("show");
        assert_eq!(count, 1);
        assert_eq!(
            history.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn history_is_capped_and_keeps_the_newest() {
        let rooms = rooms();
        for i in 0..HISTORY_MAX + 10 {
            rooms.publish("show", message(&i.to_string()));
        }
        let (_listener, history, _, _) = rooms.join("show");
        assert_eq!(history.len(), HISTORY_MAX);
        assert_eq!(history[0].text, "10");
        assert_eq!(history[HISTORY_MAX - 1].text, (HISTORY_MAX + 9).to_string());
    }

    #[test]
    fn every_message_gets_a_distinct_id_even_within_one_millisecond() {
        let rooms = rooms();
        let mut first = message("a");
        let mut second = message("b");
        first.ts = 1_000;
        second.ts = 1_000;
        let a = rooms.publish("show", first);
        let b = rooms.publish("show", second);
        assert_ne!(a.id, b.id, "ids are what both clients de-duplicate on");
    }

    #[test]
    fn a_listener_leaving_is_seen_without_a_heartbeat() {
        let rooms = rooms();
        let (first, _, _, _) = rooms.join("show");
        let (second, _, _, count) = rooms.join("show");
        assert_eq!(count, 2);
        drop(second);
        // The remaining listener is told, by the Drop impl and nothing else.
        let mut rx = first.rx.resubscribe();
        let (_third, _, _, _) = rooms.join("show");
        let seen = rx.try_recv();
        assert!(matches!(seen, Ok(Feed::Listeners(2))), "got {seen:?}");
    }

    #[test]
    fn a_room_nobody_is_in_is_forgotten_but_an_occupied_one_is_not() {
        let rooms = rooms();
        rooms.publish("stale", message("hello"));
        let (_listener, _, _, _) = rooms.join("live");
        // Far past the TTL: the empty room goes, the occupied one stays however
        // long it has been quiet.
        assert_eq!(rooms.sweep(now_ms() + IDLE_TTL_MS * 2), 1);
        let (count, listeners) = rooms.census();
        assert_eq!((count, listeners), (1, 1));
    }

    #[test]
    fn publishing_a_stream_reaches_everyone_already_in_the_room() {
        let rooms = rooms();
        let (listener, _, stream, _) = rooms.join("show");
        assert_eq!(stream, None, "a room with no broadcast has no player");

        let live = Stream {
            url: "https://radio.example.com/live.mp3".into(),
            name: "Night Owl".into(),
        };
        rooms.set_stream("show", Some(live.clone()));
        let mut rx = listener.rx.resubscribe();
        // Saying the same thing twice must not wake the room again.
        rooms.set_stream("show", Some(live.clone()));
        assert!(rx.try_recv().is_err(), "an unchanged stream was re-announced");

        rooms.set_stream("show", None);
        assert!(matches!(rx.try_recv(), Ok(Feed::Stream(None))));

        // And whoever opens the page next is told without waiting for a change.
        rooms.set_stream("show", Some(live.clone()));
        let (_later, _, stream, _) = rooms.join("show");
        assert_eq!(stream, Some(live));
    }

    #[test]
    fn a_flooder_does_not_get_a_fresh_budget_by_switching_rooms() {
        let rooms = rooms();
        for i in 0..RATE {
            assert_eq!(
                rooms.check_rate("1.2.3.4|me", "1.2.3.4", false, 1000 + i as u64),
                crate::limit::Verdict::Allowed
            );
        }
        assert!(matches!(
            rooms.check_rate("1.2.3.4|me", "1.2.3.4", false, 1006),
            crate::limit::Verdict::Blocked { .. }
        ));
    }

    #[test]
    fn a_new_token_buys_a_new_name_but_not_a_new_spam_budget() {
        let rooms = rooms();
        // Four individual budgets' worth from one address: the address ceiling
        // (RATE * 4) is what a client regenerating its token runs into.
        let mut sent = 0;
        for attempt in 0..100u64 {
            let identity = format!("1.2.3.4|token{attempt}");
            if rooms.check_rate(&identity, "1.2.3.4", false, 1000) == crate::limit::Verdict::Allowed
            {
                sent += 1;
            }
        }
        assert_eq!(sent, RATE * 4);
        // A different address is untouched by it.
        assert_eq!(
            rooms.check_rate("5.6.7.8|me", "5.6.7.8", false, 1000),
            crate::limit::Verdict::Allowed
        );
    }

    #[test]
    fn being_throttled_for_a_neighbour_does_not_cost_you_your_own_budget() {
        let rooms = rooms();
        // Somebody else behind the same address burns the whole ceiling.
        for i in 0..RATE * 4 {
            rooms.check_rate("1.2.3.4|them", "1.2.3.4", false, 1000 + i as u64);
        }
        assert!(matches!(
            rooms.check_rate("1.2.3.4|me", "1.2.3.4", false, 1100),
            crate::limit::Verdict::Blocked { .. }
        ));
        // Once the address window clears, our own five are all still there.
        for i in 0..RATE {
            assert_eq!(
                rooms.check_rate("1.2.3.4|me", "1.2.3.4", false, 20_000 + i as u64),
                crate::limit::Verdict::Allowed
            );
        }
    }

    #[test]
    fn a_name_is_held_by_whoever_claimed_it_first() {
        let rooms = rooms();
        assert_eq!(
            rooms.resolve_nick("show", "1.1.1.1", "Alice", false, 0),
            Ok("Alice".to_string())
        );
        // Somebody else cannot speak as Alice, however they spell it.
        assert_eq!(
            rooms.resolve_nick("show", "2.2.2.2", "alice", false, 0),
            Err((Reject::NameTaken, 0))
        );
        // And the owner keeps it, message after message.
        assert_eq!(
            rooms.resolve_nick("show", "1.1.1.1", "Alice", false, 60_000),
            Ok("Alice".to_string())
        );
    }

    #[test]
    fn a_sender_cannot_flip_names_message_to_message() {
        let rooms = rooms();
        rooms.resolve_nick("show", "1.1.1.1", "Alice", false, 0).unwrap();
        // The whole point: no answering yourself under a second name.
        assert_eq!(
            rooms.resolve_nick("show", "1.1.1.1", "Bob", false, 1_000),
            Err((Reject::NameLocked, NAME_COOLDOWN_MS - 1_000))
        );
        // Once the cooldown has run, a rename is fine and frees the old name.
        assert_eq!(
            rooms.resolve_nick("show", "1.1.1.1", "Bob", false, NAME_COOLDOWN_MS),
            Ok("Bob".to_string())
        );
        assert_eq!(
            rooms.resolve_nick("show", "2.2.2.2", "Alice", false, NAME_COOLDOWN_MS),
            Ok("Alice".to_string())
        );
    }

    #[test]
    fn sending_does_not_restart_the_cooldown() {
        let rooms = rooms();
        rooms.resolve_nick("show", "1.1.1.1", "Alice", false, 0).unwrap();
        // Talking throughout the window must not push the rename out forever --
        // refreshing `claimed_at` on every message would make the name permanent.
        for t in (0..NAME_COOLDOWN_MS).step_by(30_000) {
            rooms.resolve_nick("show", "1.1.1.1", "Alice", false, t).unwrap();
        }
        assert_eq!(
            rooms.resolve_nick("show", "1.1.1.1", "Bob", false, NAME_COOLDOWN_MS),
            Ok("Bob".to_string())
        );
    }

    #[test]
    fn the_host_can_take_back_their_own_name() {
        let rooms = rooms();
        // A troll claims the broadcaster's name before the show starts.
        rooms.resolve_nick("show", "9.9.9.9", "DJ Sam", false, 0).unwrap();
        // The host key is proof, so it is seized rather than refused.
        assert_eq!(
            rooms.resolve_nick("show", "1.1.1.1", "DJ Sam", true, 1_000),
            Ok("DJ Sam".to_string())
        );
        // And the troll no longer holds it.
        assert_eq!(
            rooms.resolve_nick("show", "9.9.9.9", "DJ Sam", false, 2_000),
            Err((Reject::NameTaken, 0))
        );
    }

    #[test]
    fn nobody_owns_the_anonymous_default() {
        let rooms = rooms();
        // Everyone who has not set a name shares it, so it can never be taken.
        for sender in ["1.1.1.1", "2.2.2.2", "3.3.3.3"] {
            assert_eq!(
                rooms.resolve_nick("show", sender, ANONYMOUS, false, 0),
                Ok(ANONYMOUS.to_string())
            );
        }
    }

    #[test]
    fn going_anonymous_is_not_a_way_round_the_cooldown() {
        let rooms = rooms();
        rooms.resolve_nick("show", "1.1.1.1", "Alice", false, 0).unwrap();
        // Clearing the name field used to drop the claim, which bought an
        // immediate rename -- one unnamed message and the cooldown was gone.
        rooms.resolve_nick("show", "1.1.1.1", ANONYMOUS, false, 1_000).unwrap();
        assert_eq!(
            rooms.resolve_nick("show", "1.1.1.1", "Bob", false, 2_000),
            Err((Reject::NameLocked, NAME_COOLDOWN_MS - 2_000))
        );
        // And the name is still theirs, not free for the taking.
        assert_eq!(
            rooms.resolve_nick("show", "2.2.2.2", "Alice", false, 2_000),
            Err((Reject::NameTaken, 0))
        );
    }

    #[test]
    fn a_name_is_released_once_its_owner_has_gone_quiet() {
        let rooms = rooms();
        rooms.resolve_nick("show", "1.1.1.1", "Alice", false, 0).unwrap();
        let (_listener, _, _, _) = rooms.join("show"); // keep the room alive
        rooms.sweep(NAME_TTL_MS + 1);
        assert_eq!(
            rooms.resolve_nick("show", "2.2.2.2", "Alice", false, NAME_TTL_MS + 1),
            Ok("Alice".to_string())
        );
    }

    #[test]
    fn the_host_has_a_budget_of_their_own() {
        let rooms = rooms();
        for i in 0..RATE {
            rooms.check_rate("1.2.3.4|me", "1.2.3.4", false, 1000 + i as u64);
        }
        // Same keys, host budget: still allowed, because a now-playing
        // announcement must not be dropped because the host was also typing.
        assert_eq!(
            rooms.check_rate("1.2.3.4|me", "1.2.3.4", true, 1006),
            crate::limit::Verdict::Allowed
        );
    }
}
