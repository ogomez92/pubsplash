//! The wire format, and the validation that decides what may enter a room.
//!
//! This module is the protocol. Two very different clients read it -- the
//! listener page in `assets/chat.html` and Pubsplash's `net::pubchat` -- so a
//! change here is a change to both, and the field names are `camelCase` because
//! the browser is the client that cannot rename them.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Longest message body accepted, in characters (not bytes -- a limit counted
/// in bytes silently costs a Japanese or emoji-using listener two thirds of
/// their message).
pub const TEXT_MAX: usize = 2000;

/// Longest nickname accepted, in characters.
pub const NICK_MAX: usize = 32;

/// What a nickname falls back to. Deliberately not blank: an unnamed message
/// still has to read as *somebody* in a screen reader's announcement, which is
/// built as "{nick}: {text}".
pub const ANONYMOUS: &str = "Someone";

/// Epoch milliseconds, which is what every timestamp on this wire is.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One message as it appears in history and on the feed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub nick: String,
    pub text: String,
    /// Epoch milliseconds, stamped when the server accepted it. The clients
    /// render this relative ("2 minutes ago"), so a listener whose clock is
    /// wrong sees times that are wrong by the same amount and consistent with
    /// each other, rather than a mix.
    pub ts: u64,
    /// True when the sender held the room's host key -- the broadcaster. The
    /// page marks these visually *and* in the spoken announcement, because a
    /// listener who cannot see the styling still needs to know who is talking.
    #[serde(default)]
    pub host: bool,
    /// Set on server-generated lines (a host announcement relayed from
    /// Pubsplash, a room notice). A system entry has no `nick` worth reading,
    /// so the clients announce `text` alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
}

/// Where the room's audience can hear the show.
///
/// The chat server does not and cannot know this: it is not part of a chat room
/// and it changes every time a broadcast starts. The broadcaster's app publishes
/// it with the host key (`PUT /r/{room}/stream`), which is why this is the one
/// piece of room state that needs authenticating to write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stream {
    /// A direct listen URL -- an Icecast mount, usually.
    pub url: String,
    /// What to call it. The stream title, when there is one.
    #[serde(default)]
    pub name: String,
}

/// Longest stream URL accepted.
pub const URL_MAX: usize = 2000;

/// Checks a published stream URL.
///
/// `http` and `https` only. The page puts this straight into an `<audio>`
/// element's `src`, so anything else is a scheme the browser would either
/// refuse or, worse, treat as something other than audio -- and the value
/// arrives over the network, host key or not.
pub fn clean_stream(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() || url.len() > URL_MAX {
        return None;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    // A control character in a URL is either an encoding accident or an attempt
    // at header splitting somewhere downstream. Neither is a URL worth keeping.
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    Some(url.to_string())
}

/// What a client POSTs to `/r/{room}/messages`.
#[derive(Debug, Clone, Deserialize)]
pub struct Post {
    #[serde(default)]
    pub nick: String,
    pub text: String,
    /// A random id the browser generates once and keeps, identifying this
    /// person *within* their network. It is not a credential and is trivially
    /// regenerated -- which is exactly why it decides only who holds a
    /// nickname, never how many messages anybody may send.
    ///
    /// Without it the sender is their IP address, and a nickname would then be
    /// held by a whole household -- or, behind a mobile carrier's CGNAT, by
    /// thousands of unrelated people, of whom only the first could ever pick a
    /// name.
    #[serde(default)]
    pub client: String,
}

/// Why a post was refused. The string form is the `error` field on the wire and
/// is matched by both clients, so these are protocol constants and not prose --
/// each client renders its own translated sentence from the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Empty,
    TooLong,
    RateLimited,
    BadRoom,
    BadHostKey,
    /// Somebody else in this room is already speaking under that name.
    NameTaken,
    /// The sender is trying to change their own name again too soon. This is
    /// the anti-impersonation rule: without it a nickname is a free-text field
    /// on every single message, so one person can answer themselves as three
    /// people, or borrow the name of whoever spoke last, message by message.
    NameLocked,
}

impl Reject {
    pub fn code(self) -> &'static str {
        match self {
            Reject::Empty => "empty",
            Reject::TooLong => "too_long",
            Reject::RateLimited => "rate_limited",
            Reject::BadRoom => "bad_room",
            Reject::BadHostKey => "bad_host_key",
            Reject::NameTaken => "name_taken",
            Reject::NameLocked => "name_locked",
        }
    }

    /// The HTTP status that carries it. `429` for the limiter specifically, so
    /// a generic HTTP client (curl, a station's own script) can tell a flood
    /// from a mistake without parsing the body.
    pub fn status(self) -> u16 {
        match self {
            Reject::RateLimited => 429,
            Reject::BadHostKey => 403,
            // 409: the request is well-formed, it conflicts with the room's
            // current state. A client can tell "fix your message" from "that
            // name is not yours" without reading the body.
            Reject::NameTaken | Reject::NameLocked => 409,
            _ => 400,
        }
    }
}

/// Normalizes a room name, or refuses it.
///
/// Lowercased and restricted to a URL-safe alphabet, which makes the name in
/// the address bar and the name in the room table the same string -- so
/// `/r/MyShow` and `/r/myshow` are one room rather than two that look identical
/// when read aloud.
pub fn clean_room(raw: &str) -> Option<String> {
    let name: String = raw.trim().to_ascii_lowercase();
    if name.is_empty() || name.len() > 64 {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(name)
}

/// Normalizes a nickname. Never fails: an unusable one becomes [`ANONYMOUS`]
/// rather than refusing the message, because a listener who has not set a name
/// still has something to say.
///
/// Control characters are stripped rather than escaped. They are not
/// displayable, and a name carrying a newline would otherwise break the one
/// line-per-message shape both clients rely on -- and, in a screen reader,
/// read as an unexplained pause in the middle of somebody's name.
pub fn clean_nick(raw: &str) -> String {
    let collapsed = collapse(raw);
    if collapsed.is_empty() {
        return ANONYMOUS.to_string();
    }
    collapsed.chars().take(NICK_MAX).collect()
}

/// Normalizes a client id: an opaque token, or nothing.
///
/// Restricted rather than trusted, because it lands in a map key: anything
/// outside this alphabet, or longer than a UUID needs, is discarded and the
/// sender falls back to being their address alone.
pub fn clean_client(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 64 {
        return String::new();
    }
    if raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        raw.to_string()
    } else {
        String::new()
    }
}

/// Normalizes a message body, or says why it cannot be sent.
///
/// Interior newlines survive (the composer is multiline and Shift+Enter is a
/// real newline), but every other control character is dropped and runs of
/// spaces are collapsed -- the usual way a flood is dressed up as one message.
pub fn clean_text(raw: &str) -> Result<String, Reject> {
    let mut out = String::with_capacity(raw.len());
    let mut blank_run = 0usize;
    for line in raw.replace("\r\n", "\n").split('\n') {
        let line = collapse(line);
        if line.is_empty() {
            // Three blank lines in a row is a scroll attack, not formatting:
            // it pushes the room's history off the top of everyone's list.
            blank_run += 1;
            if blank_run > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    let out = out.trim_end().to_string();
    if out.is_empty() {
        return Err(Reject::Empty);
    }
    if out.chars().count() > TEXT_MAX {
        return Err(Reject::TooLong);
    }
    Ok(out)
}

/// Drops control characters and collapses whitespace runs to a single space.
fn collapse(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_space = false;
    for c in raw.chars() {
        if c.is_control() {
            continue;
        }
        // `is_whitespace` rather than `== ' '`: a non-breaking space and the
        // various Unicode spaces are exactly what a flood uses to make an
        // "empty" message that is not empty.
        if c.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_url_must_be_something_an_audio_element_can_play() {
        assert_eq!(
            clean_stream(" https://radio.example.com/live.mp3 "),
            Some("https://radio.example.com/live.mp3".to_string())
        );
        assert_eq!(
            clean_stream("http://radio.example.com:8000/live"),
            Some("http://radio.example.com:8000/live".to_string())
        );
        assert_eq!(clean_stream(""), None);
        // It goes into an `<audio src>`, and these are not audio.
        assert_eq!(clean_stream("javascript:alert(1)"), None);
        assert_eq!(clean_stream("data:audio/mp3;base64,AAAA"), None);
        assert_eq!(clean_stream("file:///etc/passwd"), None);
        assert_eq!(clean_stream("//radio.example.com/live"), None);
        assert_eq!(clean_stream("https://a.example.com/x\r\nHost: b"), None);
        assert_eq!(clean_stream(&format!("https://a/{}", "x".repeat(URL_MAX))), None);
    }

    #[test]
    fn a_client_id_is_restricted_rather_than_trusted() {
        assert_eq!(clean_client(" abc-123_XY "), "abc-123_XY");
        assert_eq!(clean_client(""), "");
        // It becomes a map key, so nothing that could collide with the
        // separator or run away in length survives.
        assert_eq!(clean_client("a|b"), "");
        assert_eq!(clean_client("a b"), "");
        assert_eq!(clean_client(&"x".repeat(65)), "");
    }

    #[test]
    fn room_names_are_lowercased_and_restricted() {
        assert_eq!(clean_room("  MyShow "), Some("myshow".to_string()));
        assert_eq!(clean_room("night-owl_1"), Some("night-owl_1".to_string()));
        assert_eq!(clean_room(""), None);
        assert_eq!(clean_room("has space"), None);
        assert_eq!(clean_room("../etc/passwd"), None);
        assert_eq!(clean_room(&"x".repeat(65)), None);
    }

    #[test]
    fn a_nickname_is_never_refused_only_replaced() {
        assert_eq!(clean_nick("Alice"), "Alice");
        assert_eq!(clean_nick("   "), ANONYMOUS);
        assert_eq!(clean_nick("\u{0}\u{1}"), ANONYMOUS);
        assert_eq!(clean_nick("A   B"), "A B");
        assert_eq!(clean_nick(&"n".repeat(100)).chars().count(), NICK_MAX);
    }

    #[test]
    fn a_newline_survives_but_a_control_character_does_not() {
        assert_eq!(clean_text("one\ntwo").unwrap(), "one\ntwo");
        assert_eq!(clean_text("a\u{7}b").unwrap(), "ab");
        assert_eq!(clean_text("  hi   there  ").unwrap(), "hi there");
    }

    #[test]
    fn whitespace_only_messages_are_empty_however_they_are_spelled() {
        assert_eq!(clean_text("").unwrap_err(), Reject::Empty);
        assert_eq!(clean_text("   \n\n  ").unwrap_err(), Reject::Empty);
        // A non-breaking space reads as text to a naive trim, which is the
        // whole reason `collapse` tests `is_whitespace`.
        assert_eq!(clean_text("\u{00a0}\u{00a0}").unwrap_err(), Reject::Empty);
    }

    #[test]
    fn a_wall_of_blank_lines_collapses_to_one() {
        assert_eq!(clean_text("a\n\n\n\n\n\nb").unwrap(), "a\n\nb");
    }

    #[test]
    fn the_length_cap_counts_characters_not_bytes() {
        // Three bytes each; 2000 of them must pass, 2001 must not.
        let ok = "あ".repeat(TEXT_MAX);
        assert!(clean_text(&ok).is_ok());
        assert_eq!(
            clean_text(&"あ".repeat(TEXT_MAX + 1)).unwrap_err(),
            Reject::TooLong
        );
    }
}
