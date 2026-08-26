//! Reading a YouTube live chat, without an API key and without OAuth.
//!
//! # Why this is not the YouTube Data API
//!
//! The supported route is `liveChatMessages.list`, which costs **5 quota units
//! per call** against a project's default 10,000 a day. Polled at the interval
//! YouTube itself suggests (10 s, see [`POLL_FLOOR`]), that is 1,800 units an
//! hour, so a broadcaster gets a little under six hours of chat *per day*
//! across every session — and then chat stops, mid-show, with a
//! `quotaExceeded`. It also needs each user to create a Google Cloud project
//! and an OAuth client before chat works at all, which is a long, sighted,
//! copy-and-paste errand.
//!
//! So this reads the same feed the watch page's own chat panel reads: YouTube's
//! internal `youtubei` endpoint, unauthenticated. There is no quota, no setup,
//! and no account involved. The trade is that it is **not a supported API and
//! can change without notice** — every field below is therefore optional on the
//! way out ([`Chat::poll`] skips what it cannot understand rather than failing),
//! and a shape change degrades to "no messages" plus a log line, never a panic
//! or a dead stream.
//!
//! Reading is all this does. Sending needs a signed-in account, which needs
//! OAuth, which brings the whole quota errand back — so
//! [`NetCommand::SendChat`](super::NetCommand::SendChat) is refused for a
//! YouTube service and the Chat tab disables its input.
//!
//! # The flow
//!
//! 1. Resolve the configured [`ChannelRef`] to the video id that is live *now*.
//!    A handle is the useful form precisely because this step re-runs: the id
//!    changes with every broadcast, the handle never does.
//! 2. Fetch the watch page. It carries three things: `ytcfg`'s
//!    `INNERTUBE_API_KEY` and `INNERTUBE_CONTEXT`, and — inside `ytInitialData`
//!    — the first chat continuation token.
//! 3. POST that token to `/youtubei/v1/live_chat/get_live_chat`. The reply is a
//!    batch of actions plus the *next* token and how long to wait, so the loop
//!    is server-paced rather than a rate we invented.

use super::sse::ChatMessage;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::time::Duration;

/// Sent on every request. YouTube serves a different (and much thinner) page to
/// clients it does not recognise as a browser, and the chat continuation is one
/// of the things missing from it.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// The cookie that answers the EU cookie-consent interstitial.
///
/// Without it, a request from an EU address is redirected to
/// `consent.youtube.com` and the "watch page" that comes back is a consent form
/// with no video data in it at all — which looks exactly like "that channel is
/// not live" and is not. Set unconditionally: it costs nothing outside the EU,
/// and the address that matters is the *user's*, which we cannot know.
const CONSENT_COOKIE: &str = "SOCS=CAI";

/// Floor on the server's suggested poll interval.
///
/// The reply carries `timeoutMs`, which is what YouTube's own chat panel waits,
/// and it is normally 10 s. This guards against a zero or a missing field
/// turning the loop into a hot spin against someone else's server.
const POLL_FLOOR: Duration = Duration::from_secs(5);

/// Ceiling on the same, so a wild value cannot stall chat for minutes.
const POLL_CEILING: Duration = Duration::from_secs(30);

/// Used when the reply says nothing about when to come back.
const POLL_DEFAULT: Duration = Duration::from_secs(10);

/// How many message ids are remembered for de-duplication. See [`Seen`].
const SEEN_CAPACITY: usize = 4096;

#[derive(Debug)]
pub enum YoutubeError {
    /// The configured channel or video reference could not be understood.
    BadReference(String),
    /// The channel resolved, but nothing is live on it yet.
    NotLive,
    /// Live, but chat is switched off for this broadcast.
    ChatDisabled,
    /// The page loaded but did not contain what it always contains — the shape
    /// changed, or YouTube served something else entirely.
    Unexpected(String),
    Http(String),
}

impl std::fmt::Display for YoutubeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            YoutubeError::BadReference(what) => write!(f, "{what}"),
            YoutubeError::NotLive => f.write_str("nothing is live on that channel yet"),
            YoutubeError::ChatDisabled => f.write_str("live chat is turned off for this broadcast"),
            YoutubeError::Unexpected(what) => write!(f, "unexpected reply from YouTube: {what}"),
            YoutubeError::Http(what) => write!(f, "{what}"),
        }
    }
}

impl YoutubeError {
    /// Whether waiting and asking again could plausibly change the answer.
    ///
    /// [`YoutubeError::NotLive`] is the *expected* first answer, not a failure:
    /// YouTube takes the better part of a minute to promote an accepted RTMP
    /// source to a live watch page, so a chat reader that gave up on it would
    /// never once succeed on a stream it started itself.
    pub fn retryable(&self) -> bool {
        match self {
            YoutubeError::BadReference(_) => false,
            // Retryable for a reason that is easy to miss: chat is disabled
            // *until the broadcast starts*, so this is also a normal answer
            // during the same start-up window `NotLive` covers.
            YoutubeError::ChatDisabled => true,
            YoutubeError::NotLive | YoutubeError::Unexpected(_) | YoutubeError::Http(_) => true,
        }
    }
}

/// What the user typed into the service's chat field, once understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelRef {
    /// A specific broadcast. Fixed for the life of that video.
    Video(String),
    /// A channel, as the path segment that follows `youtube.com/`. Resolved to
    /// whatever is live on it at the time, every time.
    Channel(String),
}

impl ChannelRef {
    /// Understands the forms a user is likely to paste.
    ///
    /// Deliberately permissive: a screen-reader user copying an address bar is
    /// as likely to get `https://www.youtube.com/@name/live?foo=1` as `@name`,
    /// and the failure mode for refusing one of them is a service that silently
    /// has no chat.
    pub fn parse(raw: &str) -> Result<Self, YoutubeError> {
        let text = raw.trim().trim_end_matches('/');
        if text.is_empty() {
            return Err(YoutubeError::BadReference(
                "Enter a YouTube channel or video address.".to_string(),
            ));
        }

        // A bare handle or a bare video id, with no URL around it.
        if let Some(handle) = text.strip_prefix('@') {
            return Ok(ChannelRef::Channel(format!("@{}", first_segment(handle))));
        }
        if !text.contains('/') && !text.contains('.') {
            return if is_video_id(text) {
                Ok(ChannelRef::Video(text.to_string()))
            } else {
                Err(YoutubeError::BadReference(format!(
                    "{text:?} is not a YouTube channel handle, address, or video id. \
                     Use the channel handle, such as @yourchannel."
                )))
            };
        }

        // From here on it is a URL. Everything after the host is what matters,
        // and `youtu.be/ID` puts the video id there with no `v=` to find.
        let after_scheme = text.split_once("//").map(|(_, rest)| rest).unwrap_or(text);
        let (host, path) = after_scheme
            .split_once('/')
            .unwrap_or((after_scheme, ""));
        let host = host.to_ascii_lowercase();
        if !host.ends_with("youtube.com") && !host.ends_with("youtu.be") {
            return Err(YoutubeError::BadReference(format!(
                "{text:?} is not a YouTube address."
            )));
        }
        if host.ends_with("youtu.be") {
            let id = first_segment(path);
            return if is_video_id(id) {
                Ok(ChannelRef::Video(id.to_string()))
            } else {
                Err(YoutubeError::BadReference(format!(
                    "{text:?} does not name a video."
                )))
            };
        }

        // `watch?v=ID`, and the `live/ID` and `shorts/ID` short forms.
        if let Some(id) = query_value(path, "v").filter(|id| is_video_id(id)) {
            return Ok(ChannelRef::Video(id.to_string()));
        }
        let mut segments = path.split('/').filter(|s| !s.is_empty());
        let Some(first) = segments.next() else {
            return Err(YoutubeError::BadReference(format!(
                "{text:?} does not name a channel or a video."
            )));
        };
        let first = strip_query(first);
        // `/live/ID` is a watch URL; `/@name/live` is a channel one. Only the
        // first has a segment after `live` to look at, which is what tells them
        // apart without guessing.
        if matches!(first, "live" | "shorts" | "embed" | "v") {
            let id = segments.next().map(strip_query).unwrap_or_default();
            return if is_video_id(id) {
                Ok(ChannelRef::Video(id.to_string()))
            } else {
                Err(YoutubeError::BadReference(format!(
                    "{text:?} does not name a video."
                )))
            };
        }
        if first.starts_with('@') {
            return Ok(ChannelRef::Channel(first.to_string()));
        }
        // `/channel/UC...`, `/c/name`, `/user/name`: the kind and the name are
        // both part of the path YouTube wants back.
        if matches!(first, "channel" | "c" | "user") {
            let name = segments.next().map(strip_query).unwrap_or_default();
            if name.is_empty() {
                return Err(YoutubeError::BadReference(format!(
                    "{text:?} does not name a channel."
                )));
            }
            return Ok(ChannelRef::Channel(format!("{first}/{name}")));
        }
        Err(YoutubeError::BadReference(format!(
            "{text:?} is not a YouTube channel or video address."
        )))
    }

    /// The page whose canonical link names the video that is live now.
    fn live_url(&self) -> String {
        match self {
            ChannelRef::Video(id) => format!("https://www.youtube.com/watch?v={id}"),
            ChannelRef::Channel(path) => format!("https://www.youtube.com/{path}/live"),
        }
    }

    /// How the service list and the log refer to this.
    pub fn describe(&self) -> String {
        match self {
            ChannelRef::Video(id) => format!("video {id}"),
            ChannelRef::Channel(path) => format!("channel {path}"),
        }
    }
}

fn first_segment(text: &str) -> &str {
    strip_query(text.split('/').next().unwrap_or(text))
}

fn strip_query(text: &str) -> &str {
    text.split(['?', '#']).next().unwrap_or(text)
}

fn query_value<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(strip_query(value))
    })
}

/// YouTube video ids are exactly eleven characters of the URL-safe alphabet.
fn is_video_id(text: &str) -> bool {
    text.len() == 11
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One message, as far as the Chat tab is concerned.
pub struct ChatItem {
    pub id: String,
    pub author: String,
    pub text: String,
    /// Unix milliseconds, from the message's own `timestampUsec`.
    pub at_millis: u64,
}

impl ChatItem {
    pub fn into_message(self) -> ChatMessage {
        ChatMessage::external(self.id, self.author, self.text, self.at_millis)
    }
}

/// What one poll produced.
pub struct Batch {
    pub messages: Vec<ChatItem>,
    /// Concurrent viewers, when the reply happened to carry an update. YouTube
    /// sends these irregularly and not at all on some broadcasts, so this is
    /// `None` far more often than not.
    pub viewers: Option<u32>,
    /// How long the server asked us to wait before coming back.
    pub wait: Duration,
}

/// Remembers which message ids have already been reported.
///
/// Needed because **every** chat session opens on a backlog: the first reply to
/// a fresh continuation is the last few dozen messages, by design, so that a
/// viewer opening the panel sees context. That is right for a stream Pubsplash
/// has just joined and wrong for a reconnect, where the same messages are
/// already in the list — and the reconnect is the common case, since the token
/// is re-fetched whenever the connection drops.
///
/// A queue beside the set so the memory is bounded: a busy chat is thousands of
/// messages an hour and a broadcast can run all day.
struct Seen {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl Seen {
    fn new() -> Self {
        Self {
            ids: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Records `id`, answering whether it is new.
    fn insert(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > SEEN_CAPACITY
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
        true
    }
}

/// How far the move off YouTube's ranked "Top chat" view has got.
///
/// The switch has to be attempted from a *live* session (see
/// [`unfiltered_continuation`]), which means swapping in a token that has never
/// been proved to work. If it is refused, the feed must not simply die — a
/// ranked chat is far better than no chat — so the token that was working is
/// kept until the new one has answered once.
enum View {
    /// Not tried yet. The next reply carrying a switch token takes it.
    Ranked,
    /// A switch token is in `continuation`; `previous` is what to go back to.
    Trying { previous: String },
    /// Either the switch worked, or it failed and will not be tried again.
    Settled,
}

/// An open read of one broadcast's chat.
pub struct Chat {
    client: reqwest::Client,
    api_key: String,
    context: Value,
    continuation: String,
    view: View,
    seen: Seen,
    pub video_id: String,
}

/// Builds the HTTP client every request here uses.
///
/// Its own client rather than a shared one: these requests need a browser
/// User-Agent and the consent cookie on every hop including redirects, and
/// neither belongs on the Audiopub client sitting beside it.
pub fn client() -> Result<reqwest::Client, YoutubeError> {
    use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    headers.insert(COOKIE, HeaderValue::from_static(CONSENT_COOKIE));
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| YoutubeError::Http(e.to_string()))
}

impl Chat {
    /// Resolves the reference to a live video and opens its chat.
    pub async fn open(client: reqwest::Client, reference: &ChannelRef) -> Result<Self, YoutubeError> {
        let video_id = resolve_video(&client, reference).await?;
        let page = fetch_text(&client, &format!("https://www.youtube.com/watch?v={video_id}")).await?;

        let api_key = json_string_field(&page, "INNERTUBE_API_KEY").ok_or_else(|| {
            YoutubeError::Unexpected("the watch page carried no INNERTUBE_API_KEY".to_string())
        })?;
        let context: Value = json_object_after(&page, "\"INNERTUBE_CONTEXT\"")
            .ok_or_else(|| {
                YoutubeError::Unexpected("the watch page carried no INNERTUBE_CONTEXT".to_string())
            })
            .and_then(|raw| {
                serde_json::from_str(&raw)
                    .map_err(|e| YoutubeError::Unexpected(format!("INNERTUBE_CONTEXT: {e}")))
            })?;

        let initial = json_object_after(&page, "ytInitialData")
            .ok_or_else(|| YoutubeError::Unexpected("the watch page carried no ytInitialData".into()))?;
        let initial: Value = serde_json::from_str(&initial)
            .map_err(|e| YoutubeError::Unexpected(format!("ytInitialData: {e}")))?;
        let continuation = initial_continuation(&initial).ok_or(YoutubeError::ChatDisabled)?;

        Ok(Self {
            client,
            api_key,
            context,
            continuation,
            view: View::Ranked,
            seen: Seen::new(),
            video_id,
        })
    }

    /// One round trip: hand back what is new and how long to wait.
    ///
    /// Nothing in the reply is required. A batch with an unrecognised action, a
    /// message with no author, a missing continuation — each is skipped or
    /// substituted, because the alternative is a supported-today shape change
    /// taking chat down mid-broadcast. The one thing that *is* fatal is a
    /// missing `continuationContents`, which is YouTube saying this chat is over.
    pub async fn poll(&mut self) -> Result<Batch, YoutubeError> {
        match self.fetch().await {
            Ok(reply) => self.absorb(reply),
            // The only untrusted continuation this ever holds is the one that
            // moves off the ranked view, and it is untrusted precisely because
            // it can only be tried by using it. Putting the working one back and
            // retrying once — rather than reporting the failure — is what keeps
            // a refused switch from reading as a dead chat feed and restarting
            // the whole session every few seconds.
            Err(refused) => {
                let View::Trying { previous } = std::mem::replace(&mut self.view, View::Settled)
                else {
                    return Err(refused);
                };
                log::warn!(
                    "YouTube chat: the unfiltered view was refused ({refused}); \
                     staying on YouTube's ranked chat, which may hide messages"
                );
                self.continuation = previous;
                let reply = self.fetch().await?;
                self.absorb(reply)
            }
        }
    }

    /// One request for whatever `continuation` currently holds.
    async fn fetch(&self) -> Result<Value, YoutubeError> {
        let url = format!(
            "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat?key={}&prettyPrint=false",
            self.api_key
        );
        let body = serde_json::json!({
            "context": self.context,
            "continuation": self.continuation,
        });
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| YoutubeError::Http(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            // The body is read and quoted, because this endpoint answers a
            // malformed request with a `400` and a JSON explanation of what it
            // did not like — and without that, every way of getting the request
            // wrong looks identical from the log. Truncated because the body can
            // be a whole HTML error page when the failure is not this API's.
            let detail = response.text().await.unwrap_or_default();
            let detail: String = detail.chars().take(300).collect();
            return Err(YoutubeError::Http(format!(
                "YouTube answered {status} for the chat feed: {}",
                detail.trim()
            )));
        }
        response
            .json()
            .await
            .map_err(|e| YoutubeError::Unexpected(e.to_string()))
    }

    /// Turns one reply into a [`Batch`] and works out what to ask for next.
    fn absorb(&mut self, reply: Value) -> Result<Batch, YoutubeError> {
        let live = reply
            .pointer("/continuationContents/liveChatContinuation")
            .ok_or(YoutubeError::ChatDisabled)?;

        let mut messages = Vec::new();
        let mut viewers = None;
        for action in live
            .get("actions")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if let Some(count) = viewer_count(action) {
                viewers = Some(count);
            }
            let Some(item) = action.pointer("/addChatItemAction/item") else {
                continue;
            };
            let Some(parsed) = parse_item(item) else {
                continue;
            };
            if self.seen.insert(&parsed.id) {
                messages.push(parsed);
            }
        }

        let next = live
            .get("continuations")
            .and_then(Value::as_array)
            .and_then(|list| list.first());
        // Two shapes, both meaning "come back with this token": `invalidation`
        // for a live chat, `timed` for a replay. Either is fine to follow.
        let data = next.and_then(|entry| {
            entry
                .get("invalidationContinuationData")
                .or_else(|| entry.get("timedContinuationData"))
                .or_else(|| entry.get("reloadContinuationData"))
        });
        if let Some(token) = data
            .and_then(|d| d.get("continuation"))
            .and_then(Value::as_str)
        {
            self.continuation = token.to_string();
        }
        // A switch that was in flight has now answered, so it is settled either
        // way — this is the success path, and the failure path is in `poll`.
        if matches!(self.view, View::Trying { .. }) {
            self.view = View::Settled;
            log::info!("YouTube chat: reading the unfiltered live chat");
        }
        // Move off the ranked view at the first opportunity, replacing the
        // continuation just chosen above: this token stands in for it, and the
        // previous one is kept so a refusal can be undone.
        if matches!(self.view, View::Ranked)
            && let Some(token) = unfiltered_continuation(live)
        {
            self.view = View::Trying {
                previous: std::mem::replace(&mut self.continuation, token),
            };
        }
        let wait = data
            .and_then(|d| d.get("timeoutMs"))
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(POLL_DEFAULT)
            .clamp(POLL_FLOOR, POLL_CEILING);

        Ok(Batch {
            messages,
            viewers,
            wait,
        })
    }
}

/// Finds the video that is live on `reference` right now.
async fn resolve_video(
    client: &reqwest::Client,
    reference: &ChannelRef,
) -> Result<String, YoutubeError> {
    if let ChannelRef::Video(id) = reference {
        return Ok(id.clone());
    }
    let page = fetch_text(client, &reference.live_url()).await?;
    // The canonical link is the reliable one. A channel's `/live` page for a
    // channel that is *not* live is the channel page, whose canonical link
    // points at the channel rather than a watch URL — so a canonical watch URL
    // is itself the proof that something is live, and the absence of one is
    // `NotLive` rather than an error.
    let Some(id) = canonical_video_id(&page) else {
        return Err(YoutubeError::NotLive);
    };
    Ok(id)
}

fn canonical_video_id(page: &str) -> Option<String> {
    let marker = "<link rel=\"canonical\" href=\"https://www.youtube.com/watch?v=";
    let start = page.find(marker)? + marker.len();
    let rest = &page[start..];
    let end = rest.find('"')?;
    let id = &rest[..end];
    is_video_id(id).then(|| id.to_string())
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, YoutubeError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| YoutubeError::Http(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(YoutubeError::Http(format!(
            "YouTube answered {status} for {url}"
        )));
    }
    response
        .text()
        .await
        .map_err(|e| YoutubeError::Http(e.to_string()))
}

/// The chat continuation that bootstraps a session, from a live watch page.
///
/// **Only the top-level one works here.** The renderer also carries a view
/// selector with a "Top chat"/"Live chat" pair, and reaching for the "Live chat"
/// one looks like the obvious way to get the unfiltered feed — but on the watch
/// page those are 32-byte stubs, and the endpoint answers one with `400 Request
/// contains an invalid argument`. Only after a session exists does the *reply*
/// carry usable switch tokens, which is what [`unfiltered_continuation`] reads.
fn initial_continuation(initial: &Value) -> Option<String> {
    initial
        .pointer(
            "/contents/twoColumnWatchNextResults/conversationBar/liveChatRenderer\
             /continuations/0/reloadContinuationData/continuation",
        )
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The "Live chat" switch token from a `get_live_chat` reply, if it is not
/// already the view being shown.
///
/// A session bootstrapped from the watch page lands on **Top chat**, which is
/// YouTube's own ranking of the conversation and quietly drops messages. That is
/// the right default for a viewer and the wrong one for the person running the
/// broadcast, who is reading their chat out loud and cannot afford for a
/// question to be ranked away. `None` when the reply is already unfiltered, or
/// when the selector is not there at all.
fn unfiltered_continuation(live: &Value) -> Option<String> {
    let items = live
        .pointer(
            "/header/liveChatHeaderRenderer/viewSelector\
             /sortFilterSubMenuRenderer/subMenuItems",
        )?
        .as_array()?;
    let item = items.iter().find(|item| {
        item.get("title")
            .and_then(Value::as_str)
            .is_some_and(|title| title.eq_ignore_ascii_case("live chat"))
    })?;
    if item.get("selected").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    item.pointer("/continuation/reloadContinuationData/continuation")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Turns one chat item into a message, or `None` if it is not one we show.
///
/// The renderer name is the item's only key, and there are a dozen of them —
/// polls, gift announcements, "welcome to live chat" banners. Matching the four
/// that carry something a broadcaster would read out keeps the list to chat.
fn parse_item(item: &Value) -> Option<ChatItem> {
    let (kind, body) = item.as_object()?.iter().next()?;
    let (prefix, message_key) = match kind.as_str() {
        "liveChatTextMessageRenderer" => (String::new(), "message"),
        // A Super Chat's amount is the point of it, so it leads the line.
        "liveChatPaidMessageRenderer" | "liveChatPaidStickerRenderer" => {
            let amount = body
                .get("purchaseAmountText")
                .and_then(simple_text)
                .unwrap_or_default();
            (
                if amount.is_empty() {
                    String::new()
                } else {
                    format!("[{amount}] ")
                },
                "message",
            )
        }
        // A membership item has no user-written message; its text is the
        // announcement itself, in `headerSubtext`.
        "liveChatMembershipItemRenderer" => (String::new(), "headerSubtext"),
        _ => return None,
    };

    let id = body.get("id").and_then(Value::as_str)?.to_string();
    let author = body
        .get("authorName")
        .and_then(simple_text)
        .unwrap_or_else(|| "Someone".to_string());
    let text = body
        .get(message_key)
        .and_then(runs_text)
        .or_else(|| body.get("headerSubtext").and_then(runs_text))
        .unwrap_or_default();
    let text = format!("{prefix}{text}");
    if text.trim().is_empty() {
        return None;
    }
    let at_millis = body
        .get("timestampUsec")
        .and_then(Value::as_str)
        .and_then(|micros| micros.parse::<u64>().ok())
        .map(|micros| micros / 1_000)
        .unwrap_or(0);
    Some(ChatItem {
        id,
        author,
        text,
        at_millis,
    })
}

/// `{"simpleText": "..."}` or `{"runs": [...]}` — YouTube uses both for the
/// same field depending on whether it needed formatting.
fn simple_text(value: &Value) -> Option<String> {
    value
        .get("simpleText")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| runs_text(value))
}

/// Flattens a `runs` array to plain text.
///
/// Emoji runs carry no text of their own, so they are spoken by their
/// accessibility label where YouTube provides one (`"smiling face"`) and by
/// their `:shortcut:` otherwise — which is what a custom channel emote has.
/// Dropping them silently would turn an all-emoji message into an empty one.
fn runs_text(value: &Value) -> Option<String> {
    let runs = value.get("runs")?.as_array()?;
    let mut out = String::new();
    for run in runs {
        if let Some(text) = run.get("text").and_then(Value::as_str) {
            out.push_str(text);
            continue;
        }
        let Some(emoji) = run.get("emoji") else {
            continue;
        };
        let label = emoji
            .pointer("/image/accessibility/accessibilityData/label")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                emoji
                    .pointer("/shortcuts/0")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if let Some(label) = label {
            out.push_str(&label);
        }
    }
    Some(out)
}

/// The concurrent-viewer count, when an action happens to carry one.
fn viewer_count(action: &Value) -> Option<u32> {
    let view = action.pointer("/updateViewershipAction/viewCount/videoViewCountRenderer")?;
    let text = view
        .get("viewCount")
        .and_then(simple_text)
        .or_else(|| view.get("originalViewCount").and_then(Value::as_str).map(str::to_string))?;
    // "1,234 watching now" in the user's locale, so everything that is not a
    // digit goes. A locale that groups with `.` would otherwise multiply the
    // count by a thousand.
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Reads a bare JSON string field out of the page's inline script.
///
/// `"INNERTUBE_API_KEY":"AIza..."` — a substring search rather than a parse
/// because the surrounding `ytcfg.set({...})` call is a megabyte of JSON we have
/// no reason to build a `Value` for.
fn json_string_field(page: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\":\"");
    let start = page.find(&marker)? + marker.len();
    let rest = &page[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extracts the first complete JSON object at or after `marker`.
///
/// Brace-matching rather than a regex because these objects nest to a dozen
/// levels and contain braces inside string literals — `"{"` in a chat message is
/// enough to end a lazy regex match in the wrong place, and the failure is
/// silent (a truncated object that will not parse). Strings and their escapes
/// are tracked for exactly that reason.
fn json_object_after(page: &str, marker: &str) -> Option<String> {
    let from = page.find(marker)? + marker.len();
    let bytes = page.as_bytes();
    let start = from + bytes[from..].iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(page[start..start + offset + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// The watch URL a listener would use, for the Home tab and the Mastodon
/// announcement template.
pub fn watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

/// A YouTube broadcast has no server-issued stream id of the kind Audiopub
/// hands back, so one is synthesised from the video id — the same shape a direct
/// Icecast mount uses (`icecast:<mount>`) and for the same reason: `ActiveStream`
/// wants an id, and nothing outside the log ever reads it.
pub fn stream_id(video_id: &str) -> String {
    format!("youtube:{video_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn understands_the_forms_a_user_might_paste() {
        let cases = [
            ("@lofigirl", ChannelRef::Channel("@lofigirl".into())),
            ("  @lofigirl/  ", ChannelRef::Channel("@lofigirl".into())),
            (
                "https://www.youtube.com/@lofigirl/live",
                ChannelRef::Channel("@lofigirl".into()),
            ),
            (
                "https://youtube.com/@lofigirl/streams?view=2",
                ChannelRef::Channel("@lofigirl".into()),
            ),
            (
                "https://www.youtube.com/channel/UCSJ4gkVC6NrvII8umztf0Ow",
                ChannelRef::Channel("channel/UCSJ4gkVC6NrvII8umztf0Ow".into()),
            ),
            (
                "https://www.youtube.com/user/somebody",
                ChannelRef::Channel("user/somebody".into()),
            ),
            ("jfKfPfyJRdk", ChannelRef::Video("jfKfPfyJRdk".into())),
            (
                "https://www.youtube.com/watch?v=jfKfPfyJRdk&t=90",
                ChannelRef::Video("jfKfPfyJRdk".into()),
            ),
            (
                "https://youtu.be/jfKfPfyJRdk?si=abc",
                ChannelRef::Video("jfKfPfyJRdk".into()),
            ),
            (
                "https://www.youtube.com/live/jfKfPfyJRdk",
                ChannelRef::Video("jfKfPfyJRdk".into()),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(ChannelRef::parse(input).unwrap(), want, "parsing {input:?}");
        }
    }

    /// `/live/ID` is a watch URL and `/@name/live` is a channel one. They differ
    /// only in where `live` sits, which is exactly the kind of thing a rewrite
    /// gets backwards.
    #[test]
    fn tells_the_two_live_urls_apart() {
        assert_eq!(
            ChannelRef::parse("https://www.youtube.com/live/jfKfPfyJRdk").unwrap(),
            ChannelRef::Video("jfKfPfyJRdk".into())
        );
        assert_eq!(
            ChannelRef::parse("https://www.youtube.com/@lofigirl/live").unwrap(),
            ChannelRef::Channel("@lofigirl".into())
        );
    }

    #[test]
    fn refuses_what_it_cannot_use() {
        for bad in [
            "",
            "   ",
            "https://twitch.tv/somebody",
            "not a handle",
            "https://www.youtube.com/watch?v=short",
        ] {
            assert!(ChannelRef::parse(bad).is_err(), "should refuse {bad:?}");
        }
    }

    /// The brace matcher has to survive braces inside strings, because chat
    /// messages contain them and a truncated object fails silently.
    #[test]
    fn extracts_json_past_braces_in_strings() {
        let page = r#"junk window["ytInitialData"] = {"a":"}{","b":{"c":1}};more junk"#;
        let raw = json_object_after(page, "ytInitialData").unwrap();
        assert_eq!(raw, r#"{"a":"}{","b":{"c":1}}"#);
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["b"]["c"], 1);
    }

    #[test]
    fn extracts_an_escaped_quote_without_ending_the_string() {
        let page = r#"x = {"a":"say \" now","b":2};"#;
        let raw = json_object_after(page, "x =").unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn reads_a_text_message() {
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "abc",
                "authorName": {"simpleText": "@somebody"},
                "message": {"runs": [{"text": "hello "}, {"text": "world"}]},
                "timestampUsec": "1787748220241788"
            }
        });
        let parsed = parse_item(&item).unwrap();
        assert_eq!(parsed.id, "abc");
        assert_eq!(parsed.author, "@somebody");
        assert_eq!(parsed.text, "hello world");
        assert_eq!(parsed.at_millis, 1_787_748_220_241);
    }

    /// An all-emoji message must not come out empty: dropping the runs would
    /// make it vanish from a list a screen reader is reading.
    #[test]
    fn speaks_emoji_by_label_or_shortcut() {
        let item = serde_json::json!({
            "liveChatTextMessageRenderer": {
                "id": "e1",
                "authorName": {"simpleText": "fan"},
                "message": {"runs": [
                    {"emoji": {"image": {"accessibility": {"accessibilityData": {"label": "grinning face"}}}}},
                    {"emoji": {"shortcuts": [":_cheer:"]}}
                ]}
            }
        });
        let parsed = parse_item(&item).unwrap();
        assert_eq!(parsed.text, "grinning face:_cheer:");
    }

    #[test]
    fn a_super_chat_leads_with_its_amount() {
        let item = serde_json::json!({
            "liveChatPaidMessageRenderer": {
                "id": "s1",
                "authorName": {"simpleText": "patron"},
                "purchaseAmountText": {"simpleText": "$5.00"},
                "message": {"runs": [{"text": "great show"}]}
            }
        });
        assert_eq!(parse_item(&item).unwrap().text, "[$5.00] great show");
    }

    /// Renderers we do not show must be skipped, not turned into blank rows.
    #[test]
    fn skips_renderers_that_are_not_chat() {
        for kind in [
            "liveChatViewerEngagementMessageRenderer",
            "liveChatPlaceholderItemRenderer",
            "liveChatModeChangeMessageRenderer",
        ] {
            let item = serde_json::json!({ kind: {"id": "x"} });
            assert!(parse_item(&item).is_none(), "{kind} should be skipped");
        }
    }

    #[test]
    fn reads_a_viewer_count_out_of_a_grouped_number() {
        let action = serde_json::json!({
            "updateViewershipAction": {
                "viewCount": {
                    "videoViewCountRenderer": {
                        "viewCount": {"simpleText": "1,234 watching now"}
                    }
                }
            }
        });
        assert_eq!(viewer_count(&action), Some(1234));
    }

    /// The backlog on a reconnect is the whole reason this exists.
    #[test]
    fn remembers_ids_and_forgets_the_oldest() {
        let mut seen = Seen::new();
        assert!(seen.insert("a"));
        assert!(!seen.insert("a"));
        for n in 0..SEEN_CAPACITY {
            seen.insert(&format!("id-{n}"));
        }
        assert!(seen.insert("a"), "the oldest id should have been forgotten");
        assert!(seen.ids.len() <= SEEN_CAPACITY);
    }

    #[test]
    fn finds_the_canonical_video_id() {
        let page = r#"<link rel="canonical" href="https://www.youtube.com/watch?v=YDvsBbKfLPA">"#;
        assert_eq!(canonical_video_id(page).as_deref(), Some("YDvsBbKfLPA"));
        assert_eq!(
            canonical_video_id(r#"<link rel="canonical" href="https://www.youtube.com/@SkyNews">"#),
            None
        );
    }

    /// The watch page's view-selector tokens are stubs the endpoint refuses with
    /// `400`; only the top-level continuation bootstraps a session. Verified
    /// against the real endpoint — see `youtube_chat_reads_a_real_broadcast`.
    #[test]
    fn bootstraps_from_the_top_level_continuation_only() {
        let initial = serde_json::json!({
            "contents": {"twoColumnWatchNextResults": {"conversationBar": {"liveChatRenderer": {
                "continuations": [{"reloadContinuationData": {"continuation": "bootstrap-token"}}],
                "header": {"liveChatHeaderRenderer": {"viewSelector": {"sortFilterSubMenuRenderer": {
                    "subMenuItems": [
                        {"title": "Top chat", "continuation": {"reloadContinuationData": {"continuation": "stub-a"}}},
                        {"title": "Live chat", "continuation": {"reloadContinuationData": {"continuation": "stub-b"}}}
                    ]
                }}}}
            }}}}
        });
        assert_eq!(
            initial_continuation(&initial).as_deref(),
            Some("bootstrap-token")
        );
    }

    /// A session bootstrapped from the watch page lands on Top chat, which is
    /// ranked and hides messages. The reply's switch token is the way off it.
    #[test]
    fn takes_the_switch_to_unfiltered_chat() {
        let live = serde_json::json!({
            "header": {"liveChatHeaderRenderer": {"viewSelector": {"sortFilterSubMenuRenderer": {
                "subMenuItems": [
                    {"title": "Top chat", "selected": true, "continuation": {"reloadContinuationData": {"continuation": "ranked"}}},
                    {"title": "Live chat", "selected": false, "continuation": {"reloadContinuationData": {"continuation": "unfiltered"}}}
                ]
            }}}}
        });
        assert_eq!(
            unfiltered_continuation(&live).as_deref(),
            Some("unfiltered")
        );
    }

    /// Already unfiltered: nothing to switch to, and swapping the continuation
    /// for one naming the view we are on would cost a round trip for nothing.
    #[test]
    fn does_not_switch_when_already_unfiltered() {
        let live = serde_json::json!({
            "header": {"liveChatHeaderRenderer": {"viewSelector": {"sortFilterSubMenuRenderer": {
                "subMenuItems": [
                    {"title": "Top chat", "selected": false, "continuation": {"reloadContinuationData": {"continuation": "ranked"}}},
                    {"title": "Live chat", "selected": true, "continuation": {"reloadContinuationData": {"continuation": "unfiltered"}}}
                ]
            }}}}
        });
        assert_eq!(unfiltered_continuation(&live), None);
        // No selector at all is the other way to have nothing to do.
        assert_eq!(unfiltered_continuation(&serde_json::json!({})), None);
    }

    /// Reads a real live chat, end to end, against YouTube as it is today.
    ///
    /// The unit tests above pin the parsing against fixtures, which is what
    /// keeps a refactor honest — but this module reads an **unsupported**
    /// endpoint, so the thing most likely to break it is YouTube changing shape,
    /// and a fixture cannot notice that. This can.
    ///
    /// Takes its target from `PUBSPLASH_YOUTUBE_CHAT` (a handle, a watch URL, or
    /// a video id) rather than hard-coding one: any channel named here would
    /// stop being live, and a test that fails for that reason teaches the next
    /// reader to ignore it.
    ///
    /// ```text
    /// $env:PUBSPLASH_YOUTUBE_CHAT = "@somechannel"
    /// cargo test youtube_chat_reads_a_real_broadcast -- --include-ignored --nocapture
    /// ```
    #[test]
    #[ignore = "hits youtube.com and needs a broadcast that is live right now"]
    fn youtube_chat_reads_a_real_broadcast() {
        let Ok(target) = std::env::var("PUBSPLASH_YOUTUBE_CHAT") else {
            eprintln!("set PUBSPLASH_YOUTUBE_CHAT to a live channel or video first");
            return;
        };
        let reference = ChannelRef::parse(&target).expect("the reference parses");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = client().expect("an HTTP client");
            let mut chat = Chat::open(client, &reference)
                .await
                .unwrap_or_else(|e| panic!("opening {}: {e}", reference.describe()));
            eprintln!("reading {}", watch_url(&chat.video_id));
            let first = chat.poll().await.expect("the first poll");
            eprintln!(
                "backlog: {} messages, next poll in {:?}",
                first.messages.len(),
                first.wait
            );
            for item in first.messages.iter().take(5) {
                eprintln!("  {}: {}", item.author, item.text);
            }
            // The second poll is the one that proves the loop: it follows the
            // continuation the first reply handed back, and it must not repeat
            // the backlog.
            tokio::time::sleep(first.wait).await;
            let second = chat.poll().await.expect("the second poll");
            eprintln!("second poll: {} new messages", second.messages.len());
            let repeats = second
                .messages
                .iter()
                .filter(|item| first.messages.iter().any(|seen| seen.id == item.id))
                .count();
            assert_eq!(repeats, 0, "the backlog must not be reported twice");
        });
    }

    /// A watch page with no chat renderer means chat is off, which is a
    /// retryable answer during the minute before a broadcast goes live.
    #[test]
    fn no_renderer_means_chat_is_disabled() {
        let initial = serde_json::json!({"contents": {"twoColumnWatchNextResults": {}}});
        assert!(initial_continuation(&initial).is_none());
        assert!(YoutubeError::ChatDisabled.retryable());
        assert!(YoutubeError::NotLive.retryable());
        assert!(!YoutubeError::BadReference("x".into()).retryable());
    }
}
