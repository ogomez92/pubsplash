//! Client for a Pubsplash Chat server -- the chat an Icecast mount does not
//! have.
//!
//! Icecast is one-way. A listener has no path back to the broadcaster in the
//! protocol, the metadata or anywhere else, so a direct Icecast service has
//! always reported "chat is only available for Audiopub and YouTube services".
//! The answer is a separate little server alongside the stream: `chat/` in this
//! repository, which the operator drops on a box and points this at.
//!
//! **Deliberately the same shape as Audio Pub's feed**: SSE down, `POST` up. It
//! is why [`super::sse::SseParser`] is reused verbatim here and why
//! [`super::spawn_chat_feed`]'s supervisor -- the backoff ladder, the idle
//! watchdog, the one-line-per-outage reporting and the reconnect doorbell --
//! needed no second copy. The events are this server's own, so they are typed
//! here rather than in `sse`, which stays Audio Pub's.
//!
//! Messages reach the rest of the app as [`super::NetEvent::Chat`] through
//! [`ChatMessage::external`], exactly as YouTube's do. The Chat tab, the TTS
//! reader and the sound cues then work without knowing where chat came from.

use super::sse::{ChatMessage, SseEvent};
use crate::t;
use crate::secret::Secret;
use serde::Deserialize;
use std::time::Duration;

/// Where one service's chat lives.
#[derive(Debug, Clone)]
pub struct ChatTarget {
    /// The server's base URL with no trailing slash, e.g.
    /// `https://chat.example.com`. A sub-path mount is fine and is kept.
    base: String,
    /// The room name, already normalized the way the server normalizes it.
    room: String,
    /// What the broadcaster is called in the room.
    ///
    /// Its own setting (`SiteConfig::chat_nick`, resolved by
    /// `chat_display_name`), not the service nickname it was taken from at
    /// first. The two names are read by different people: a nickname names the
    /// service to the user, in the Connect dialog and the log, while this is
    /// what every listener sees against every line the broadcaster says -- and
    /// a name chosen to sit in a service list is rarely one to be introduced by.
    /// The chat server holds it against us for the session, so nobody else in
    /// the room can answer as the host.
    nick: String,
    /// The room's host key, which marks our messages as the broadcaster's.
    /// Empty is allowed: chat then works and our own messages simply appear as
    /// an ordinary listener's, which is a reasonable way to run a room where
    /// the operator has not handed out a key.
    key: Secret,
}

impl ChatTarget {
    /// Builds a target from what the user typed, or says what is wrong with it.
    ///
    /// The room name is normalized here rather than refused, because the user
    /// is typing the same string the server lowercases: refusing `MyShow` when
    /// `/r/myshow` is exactly where it would go is a rejection with no lesson
    /// in it. What *is* refused is a character the server would 404 on, since
    /// that fails at stream start instead, where nobody is looking.
    pub fn new(url: &str, room: &str, nick: &str, key: Secret) -> Result<Self, String> {
        let base = url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(t!("Enter the chat server address."));
        }
        // Scheme-less is the common paste, and defaulting to https is both the
        // safe choice and the one an operator behind a reverse proxy wants.
        let base = if base.starts_with("http://") || base.starts_with("https://") {
            base.to_string()
        } else {
            format!("https://{base}")
        };

        let room = room.trim().to_ascii_lowercase();
        if room.is_empty() {
            return Err(t!("Enter the chat room name."));
        }
        if room.len() > 64
            || !room
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(t!(
                "A chat room name may only use letters, digits, hyphens and underscores."
            ));
        }

        // A blank nickname is not refused: the chat server falls back to its
        // own anonymous name, and refusing to connect over a display name would
        // be a hard failure for a soft problem.
        let nick = nick.trim().to_string();
        Ok(Self {
            base,
            room,
            nick,
            key,
        })
    }

    /// How this target reads in the log. Never the key.
    pub fn describe(&self) -> String {
        format!("room {:?} on {}", self.room, self.base)
    }

    pub fn events_url(&self) -> String {
        format!("{}/r/{}/events", self.base, self.room)
    }

    /// What to post messages as.
    pub fn nick(&self) -> &str {
        &self.nick
    }

    pub fn messages_url(&self) -> String {
        format!("{}/r/{}/messages", self.base, self.room)
    }

    pub fn stream_url(&self) -> String {
        format!("{}/r/{}/stream", self.base, self.room)
    }
}

/// The HTTP client this module's requests use.
///
/// Its own, like [`super::youtube::client`]: the Audiopub client beside it
/// carries a cookie store and a session that have no business being sent to
/// somebody else's chat server. No overall timeout, because the feed is a
/// response that stays open for the whole broadcast -- the watchdog in
/// `read_chat_feed` is what notices a dead one.
pub fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("pubsplash/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

/// Opens the feed.
pub async fn open_events(
    client: &reqwest::Client,
    target: &ChatTarget,
) -> Result<reqwest::Response, String> {
    let response = client
        .get(target.events_url())
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(feed_status(status));
    }
    Ok(response)
}

/// Posts one message as the broadcaster.
pub async fn send(
    client: &reqwest::Client,
    target: &ChatTarget,
    nick: &str,
    text: &str,
) -> Result<(), String> {
    let mut request = client.post(target.messages_url()).json(&serde_json::json!({
        "nick": nick,
        "text": text,
    }));
    if !target.key.is_empty() {
        request = request.bearer_auth(target.key.as_str());
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    if response.status().is_success() {
        return Ok(());
    }
    // The server names the reason in the body; it is far more useful than the
    // status, and it is the difference between "slow down" and "your host key
    // is wrong", which the user has to fix differently.
    let status = response.status();
    let reason = response
        .json::<Refusal>()
        .await
        .ok()
        .map(|r| r.describe())
        .unwrap_or_else(|| send_status(status));
    Err(reason)
}

/// Tells the room where the show can be heard, or takes the player down.
///
/// The chat server cannot know this and must not guess it: a listen URL is not
/// part of a chat room, and it is different for every station. So the
/// broadcaster publishes it with the host key when the stream starts, and
/// clears it when the stream ends -- a play button that outlives the broadcast
/// is worse than none, because it plays nothing and explains nothing.
///
/// Never fatal. A chat server that will not take the URL leaves listeners with
/// chat and no play button, which is exactly where they were before this
/// existed, and is no reason at all to fail a broadcast that is already live.
pub async fn set_stream(
    client: &reqwest::Client,
    target: &ChatTarget,
    url: &str,
    name: &str,
) -> Result<(), String> {
    // Without a key we are an ordinary listener as far as the server is
    // concerned, and this is the one thing a listener may not write. Saying so
    // here keeps a pointless 403 out of the log every time a stream starts.
    if target.key.is_empty() {
        return Err("no chat host key is set, so the listen URL cannot be published".to_string());
    }
    let response = client
        .put(target.stream_url())
        .bearer_auth(target.key.as_str())
        .json(&serde_json::json!({ "url": url, "name": name }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status().is_success() {
        return Ok(());
    }
    Err(feed_status(response.status()))
}

/// The server's refusal body.
#[derive(Debug, Deserialize)]
struct Refusal {
    #[serde(default)]
    error: String,
    #[serde(default, rename = "retryAfterMs")]
    retry_after_ms: u64,
}

impl Refusal {
    fn describe(&self) -> String {
        match self.error.as_str() {
            "rate_limited" => {
                let seconds = (self.retry_after_ms / 1000).max(1);
                t!(
                    "the chat server is rate limiting us; try again in about {seconds} seconds",
                    seconds = seconds.to_string()
                )
            }
            "bad_host_key" => t!("the chat host key is wrong for this room"),
            "name_taken" => t!("somebody else in the room is already using that name"),
            "name_locked" => t!("the chat server is still holding our previous name"),
            "too_long" => t!("that message is too long for the chat server"),
            "empty" => t!("there is nothing to send"),
            "bad_room" => t!("the chat server has no room by that name"),
            other if !other.is_empty() => other.to_string(),
            _ => t!("the chat server refused the message"),
        }
    }
}

/// Why the *feed* would not open, for the log.
///
/// Deliberately **not** translated. It reaches `ui::chat_feed_line`, which
/// exists only to be logged, and users are asked to send their log when
/// something goes wrong -- a log the maintainer cannot read is not a diagnostic
/// (see `agents.md`). Its sibling below is the translated one, because that
/// reaches a message box.
fn feed_status(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        404 => "the chat server has no room by that name".to_string(),
        403 => "the chat host key is wrong for this room".to_string(),
        _ => format!("the chat server answered {status}"),
    }
}

/// Why a *send* was refused, when the body did not say. Shown in a message box
/// the user is waiting in front of, so this one is translated.
fn send_status(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        404 => t!("the chat server has no room by that name"),
        403 => t!("the chat host key is wrong for this room"),
        _ => t!(
            "the chat server answered {status}",
            status = status.to_string()
        ),
    }
}

/// A typed event off the feed. Unknown events are dropped, so a newer server
/// may add one without this refusing to talk to it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    /// Everything the server had when we connected, newest last.
    Hello { history: Vec<Incoming> },
    Chat(Incoming),
    /// We fell far enough behind that the server stopped keeping our backlog.
    Lagged { missed: u64 },
}

/// One message as this server sends it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Incoming {
    pub id: String,
    #[serde(default)]
    pub nick: String,
    #[serde(default)]
    pub text: String,
    /// Epoch milliseconds, as everywhere else on this wire.
    #[serde(default)]
    pub ts: u64,
    #[serde(default)]
    pub host: bool,
}

impl Incoming {
    /// Converts to the app's one chat type.
    ///
    /// The broadcaster's own messages are marked in the *name*, not by a flag:
    /// [`ChatMessage`] has nowhere to carry one, and the Chat tab and the TTS
    /// reader both work from the display name alone -- so this is what makes a
    /// host line read as the host wherever it is shown or spoken.
    pub fn into_chat(self) -> ChatMessage {
        let name = if self.host {
            t!("{nick} (broadcaster)", nick = self.nick.clone())
        } else {
            self.nick.clone()
        };
        ChatMessage::external(self.id, name, self.text, self.ts)
    }
}

impl ChatEvent {
    pub fn from_sse(raw: &SseEvent) -> Option<ChatEvent> {
        #[derive(Deserialize)]
        struct Hello {
            #[serde(default)]
            history: Vec<Incoming>,
        }
        #[derive(Deserialize)]
        struct Lagged {
            #[serde(default)]
            missed: u64,
        }

        match raw.event.as_str() {
            "hello" => serde_json::from_str::<Hello>(&raw.data)
                .ok()
                .map(|h| ChatEvent::Hello { history: h.history }),
            "chat" => serde_json::from_str::<Incoming>(&raw.data)
                .ok()
                .map(ChatEvent::Chat),
            "lagged" => serde_json::from_str::<Lagged>(&raw.data)
                .ok()
                .map(|l| ChatEvent::Lagged { missed: l.missed }),
            // `listeners` is deliberately ignored: a direct Icecast service
            // counts its audience from Icecast's own status document, and how
            // many chat pages are open is a different and worse answer to that
            // question. See `stats::stats_target`.
            _ => None,
        }
    }
}

/// Decides which of a `hello`'s replayed messages are actually new to us.
///
/// The server replays its history on **every** connect, which is right for the
/// web page and wrong for us twice over. On the first connect it would announce
/// and *speak aloud* up to a hundred messages from before the broadcast began;
/// on a reconnect it would do the same for everything already read out. But
/// simply ignoring history loses whatever was said during an outage, which is
/// exactly what the broadcaster needs to see.
///
/// So the first `hello` sets a watermark and delivers nothing, and every one
/// after it delivers only what follows the last message we saw. A watermark
/// that has fallen off the end of the server's ring means the outage was longer
/// than its history, and everything there is delivered -- being behind is worth
/// saying, and the ring is bounded.
#[derive(Debug, Default)]
pub struct Backlog {
    last_seen: Option<String>,
    greeted: bool,
}

impl Backlog {
    /// What to deliver from a `hello`.
    pub fn catch_up(&mut self, history: Vec<Incoming>) -> Vec<Incoming> {
        let first = !self.greeted;
        self.greeted = true;
        // Cloned, never taken: an *empty* replay must leave the watermark where
        // it was. Taking it meant a room whose history had aged out -- or one
        // that had simply been quiet -- wiped our place, and the reconnect after
        // that one then delivered nothing at all, losing every message sent in
        // between with no sign that anything had gone wrong.
        let watermark = self.last_seen.clone();
        if let Some(last) = history.last() {
            self.last_seen = Some(last.id.clone());
        }
        if first {
            return Vec::new();
        }
        let Some(watermark) = watermark else {
            return Vec::new();
        };
        match history.iter().position(|m| m.id == watermark) {
            Some(index) => history.into_iter().skip(index + 1).collect(),
            // The watermark aged out of the ring: we were away longer than the
            // server remembers, so everything it still has is news.
            None => history,
        }
    }

    /// Records a live message, so a later reconnect knows where we were.
    pub fn saw(&mut self, id: &str) {
        self.last_seen = Some(id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::sse::SseParser;

    fn message(id: &str) -> Incoming {
        Incoming {
            id: id.to_string(),
            nick: "Alice".into(),
            text: "hi".into(),
            ts: 1_750_000_000_000,
            host: false,
        }
    }

    #[test]
    fn a_target_is_built_from_what_a_user_would_actually_type() {
        let target = ChatTarget::new(
            "  https://chat.example.com/  ",
            " MyShow ",
            "DJ Sam",
            Secret::new("KEY"),
        )
        .unwrap();
        assert_eq!(target.events_url(), "https://chat.example.com/r/myshow/events");
        assert_eq!(
            target.messages_url(),
            "https://chat.example.com/r/myshow/messages"
        );
        assert_eq!(
            target.stream_url(),
            "https://chat.example.com/r/myshow/stream"
        );
        // A pasted address without a scheme is the common case, and https is
        // both the safe default and what a proxied deployment wants.
        let bare = ChatTarget::new("chat.example.com", "show", "DJ", Secret::default()).unwrap();
        assert_eq!(bare.events_url(), "https://chat.example.com/r/show/events");
        // A sub-path mount survives, so the server can live under a prefix.
        let sub = ChatTarget::new("https://example.com/chat", "show", "DJ", Secret::default()).unwrap();
        assert_eq!(sub.events_url(), "https://example.com/chat/r/show/events");
    }

    #[test]
    fn a_room_name_the_server_would_reject_is_refused_here_instead() {
        // Refused while the dialog is open and the user is waiting, rather than
        // at stream start where a 404 would surface as chat silently not working.
        assert!(ChatTarget::new("https://c.example.com", "has space", "DJ", Secret::default()).is_err());
        assert!(ChatTarget::new("https://c.example.com", "", "DJ", Secret::default()).is_err());
        assert!(ChatTarget::new("", "show", "DJ", Secret::default()).is_err());
        assert!(
            ChatTarget::new("https://c.example.com", &"x".repeat(65), "DJ", Secret::default()).is_err()
        );
    }

    #[test]
    fn the_key_never_reaches_the_log() {
        let target = ChatTarget::new("https://c.example.com", "show", "DJ", Secret::new("SECRETKEY"))
                .unwrap();
        assert!(!target.describe().contains("SECRETKEY"));
        assert!(!format!("{target:?}").contains("SECRETKEY"));
    }

    #[test]
    fn events_parse_off_the_shared_sse_parser() {
        let mut parser = SseParser::new();
        let raw = parser.feed(
            concat!(
                "event: hello\n",
                "data: {\"room\":\"show\",\"history\":[{\"id\":\"1-1\",\"nick\":\"Alice\",\"text\":\"hi\",\"ts\":1,\"host\":false}]}\n\n",
                "event: chat\n",
                "data: {\"id\":\"1-2\",\"nick\":\"DJ\",\"text\":\"on air\",\"ts\":2,\"host\":true}\n\n",
                "event: lagged\n",
                "data: {\"missed\":9}\n\n",
                "event: listeners\n",
                "data: {\"count\":4}\n\n",
            )
            .as_bytes(),
        );
        assert_eq!(raw.len(), 4);
        let ChatEvent::Hello { history } = ChatEvent::from_sse(&raw[0]).unwrap() else {
            panic!("expected hello");
        };
        assert_eq!(history.len(), 1);
        let ChatEvent::Chat(chat) = ChatEvent::from_sse(&raw[1]).unwrap() else {
            panic!("expected chat");
        };
        assert!(chat.host);
        assert_eq!(
            ChatEvent::from_sse(&raw[2]),
            Some(ChatEvent::Lagged { missed: 9 })
        );
        // Listener counts come from Icecast for this service; this feed's are a
        // different question wearing the same word.
        assert_eq!(ChatEvent::from_sse(&raw[3]), None);
    }

    #[test]
    fn a_host_message_says_so_in_the_name() {
        // `ChatMessage` has no flag for it, and the Chat tab and the speech
        // reader both work from the display name, so the name is where it goes.
        let chat = message("1-1").into_chat();
        assert_eq!(chat.user.display(), "Alice");
        let mut host = message("1-2");
        host.host = true;
        host.nick = "DJ Sam".into();
        assert!(host.into_chat().user.display().contains("DJ Sam"));
    }

    #[test]
    fn the_first_hello_is_never_replayed() {
        // Otherwise connecting reads a hundred messages aloud, from before the
        // broadcast even started.
        let mut backlog = Backlog::default();
        let delivered = backlog.catch_up(vec![message("1"), message("2"), message("3")]);
        assert!(delivered.is_empty());
    }

    #[test]
    fn a_reconnect_delivers_only_what_was_missed() {
        let mut backlog = Backlog::default();
        backlog.catch_up(vec![message("1"), message("2")]);
        backlog.saw("3"); // arrived live before the feed dropped
        let delivered = backlog.catch_up(vec![message("2"), message("3"), message("4"), message("5")]);
        assert_eq!(
            delivered.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["4", "5"]
        );
    }

    #[test]
    fn a_reconnect_with_nothing_missed_delivers_nothing() {
        let mut backlog = Backlog::default();
        backlog.catch_up(vec![message("1"), message("2")]);
        let delivered = backlog.catch_up(vec![message("1"), message("2")]);
        assert!(delivered.is_empty());
    }

    #[test]
    fn an_outage_longer_than_the_servers_memory_delivers_what_is_left() {
        let mut backlog = Backlog::default();
        backlog.catch_up(vec![message("1")]);
        // "1" has aged out of the ring, so we cannot know what we missed --
        // everything still there is news, and the ring is bounded.
        let delivered = backlog.catch_up(vec![message("90"), message("91")]);
        assert_eq!(
            delivered.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["90", "91"]
        );
    }

    #[test]
    fn an_empty_history_moves_nothing() {
        let mut backlog = Backlog::default();
        backlog.catch_up(vec![message("1")]);
        backlog.saw("2");
        assert!(backlog.catch_up(Vec::new()).is_empty());
        // The watermark survives an empty replay, so the next one still knows
        // where we were.
        let delivered = backlog.catch_up(vec![message("2"), message("3")]);
        assert_eq!(
            delivered.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["3"]
        );
    }

    #[test]
    fn a_refusal_is_reported_by_its_reason_not_its_status() {
        let refusal: Refusal =
            serde_json::from_str(r#"{"ok":false,"error":"bad_host_key"}"#).unwrap();
        assert!(refusal.describe().contains("host key"));
        let slow: Refusal =
            serde_json::from_str(r#"{"ok":false,"error":"rate_limited","retryAfterMs":4200}"#)
                .unwrap();
        assert!(slow.describe().contains('4'));
    }
}
