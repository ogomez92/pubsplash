//! Pubsplash Chat: the return channel an Icecast broadcast does not have.
//!
//! Icecast carries audio one way and nothing back, so a listener has no way to
//! reach the broadcaster at all. This is the smallest thing that fixes that: one
//! binary, no database, no accounts, no build step. The operator copies it to a
//! server, points a reverse proxy at it, and every room is live the moment
//! somebody opens its URL.
//!
//! The shape is deliberately boring. **SSE down, `POST` up** -- no WebSocket --
//! because that survives every corporate proxy, needs no upgrade handling in
//! the reverse proxy, and reuses the SSE parser and reconnect machinery
//! Pubsplash already has for Audio Pub's feed. The listener page is one file
//! compiled into the binary, so there is nothing to serve from disk and nothing
//! to get out of step with the server it talks to.
//!
//! Routes:
//!
//! | Method | Path                  | What it is                             |
//! |--------|-----------------------|----------------------------------------|
//! | GET    | `/`                   | Room picker                            |
//! | GET    | `/r/{room}`           | The listener page                      |
//! | GET    | `/r/{room}/events`    | The feed (SSE)                         |
//! | POST   | `/r/{room}/messages`  | Send one message                       |
//! | GET    | `/healthz`            | `{ok, rooms, listeners}`               |
//!
//! Rooms are created by being visited and forgotten when nobody has visited for
//! an hour. See `room.rs` for why that is the right lifecycle here, `limit.rs`
//! for the spam guard, and `hostkey.rs` for how the broadcaster is recognised
//! without anything being stored about them.

mod hostkey;
mod limit;
mod message;
mod room;

use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::stream::Stream;
use hostkey::Secret;
use limit::{HOST_RATE, IP_RATE, Limiter, RATE, Verdict, WINDOW_MS};
use message::{
    Message, Post, Reject, Stream as StreamInfo, clean_client, clean_nick, clean_room,
    clean_stream,
    clean_text, now_ms,
};
use rand::RngCore;
use room::{Feed, Rooms};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

/// The listener page and the room picker, compiled in. Nothing is served from
/// disk: a deployment is one file, and the page can never be a version behind
/// the server that answers it.
const CHAT_PAGE: &str = include_str!("../assets/chat.html");
const INDEX_PAGE: &str = include_str!("../assets/index.html");

/// How often idle rooms and spent rate-limit windows are swept.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How often the feed emits a keepalive comment. Well inside the 90 s idle
/// timeout Pubsplash's chat reader gives up after, and inside the 60 s most
/// reverse proxies default to for an idle upstream response.
const KEEPALIVE: Duration = Duration::from_secs(15);

struct AppState {
    rooms: Rooms,
    secret: Secret,
    /// Shown in the page's heading and title. The station's name, usually.
    brand: String,
    /// The page's fallback language, for an audience whose browsers are not set
    /// the way the page would otherwise assume. It is the LAST resort, below
    /// the listener's own `?lang=` and their browser's preferences, because a
    /// listener's own setting is more specific to them than the station's is.
    lang: String,
    /// The limiter's settings, echoed to clients on `hello` so a page never
    /// carries its own copy of the server's budget. SonicRoom's client keeps
    /// the two in step by hand in two files; sending it is what removes the
    /// chance of them disagreeing after an operator raises `--rate`.
    rate: usize,
    window_ms: u64,
    /// Whether to believe `X-Forwarded-For`.
    ///
    /// Off by default and deliberately so: the rate limiter is keyed on the
    /// client address, and a server that trusts a header it is handed lets
    /// anybody mint a fresh budget per message by inventing an address. It is
    /// only correct behind a reverse proxy that *sets* the header, which is
    /// also the only deployment where the socket address is useless.
    trust_proxy: bool,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = run(args).await {
        eprintln!("pubsplash-chat: {message}");
        std::process::exit(1);
    }
}

async fn run(args: Vec<String>) -> Result<(), String> {
    let mut bind = "0.0.0.0:8080".to_string();
    let mut secret_file = "pubsplash-chat.secret".to_string();
    let mut secret_value: Option<String> = std::env::var("PUBSPLASH_CHAT_SECRET").ok();
    let mut brand = "Chat".to_string();
    let mut lang = "en".to_string();
    let mut trust_proxy = false;
    let mut rate = RATE;
    let mut window_ms = WINDOW_MS;
    let mut key_for: Option<String> = None;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next()
                .ok_or_else(|| format!("{name} needs a value. Try --help."))
        };
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                return Ok(());
            }
            "--version" | "-V" => {
                println!("pubsplash-chat {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "key" => {
                key_for = Some(value("key")?);
            }
            "--bind" => bind = value("--bind")?,
            "--secret-file" => secret_file = value("--secret-file")?,
            "--secret" => secret_value = Some(value("--secret")?),
            "--brand" => brand = value("--brand")?,
            "--lang" => lang = value("--lang")?,
            "--trust-proxy" => trust_proxy = true,
            "--rate" => {
                rate = value("--rate")?
                    .parse()
                    .map_err(|_| "--rate takes a whole number of messages".to_string())?;
            }
            "--rate-window" => {
                let seconds: u64 = value("--rate-window")?
                    .parse()
                    .map_err(|_| "--rate-window takes a whole number of seconds".to_string())?;
                window_ms = seconds.saturating_mul(1000);
            }
            other => return Err(format!("unknown option {other:?}. Try --help.")),
        }
    }

    let secret = load_secret(secret_value.as_deref(), &secret_file)?;

    // `key <room>` is the whole of the setup ritual: it prints what the
    // broadcaster pastes into Pubsplash, and exits without opening a socket.
    if let Some(room) = key_for {
        let room = clean_room(&room).ok_or_else(|| {
            format!("{room:?} is not a usable room name. Use letters, digits, - and _.")
        })?;
        println!("{}", secret.key_for(&room));
        return Ok(());
    }

    if rate == 0 {
        return Err("--rate must be at least 1, or nobody can say anything".into());
    }

    let state = Arc::new(AppState {
        rooms: Rooms::new(
            Limiter::new(rate, window_ms),
            // The host's budget scales with the room's, so lowering the room
            // rate does not accidentally throttle the broadcaster's app below
            // what it needs for now-playing announcements.
            Limiter::new(rate.max(HOST_RATE), window_ms),
            // The address-wide ceiling, kept in the same proportion to the
            // per-sender budget when an operator changes `--rate`.
            Limiter::new(rate.saturating_mul(IP_RATE / RATE), window_ms),
        ),
        secret,
        brand,
        lang,
        rate,
        window_ms,
        trust_proxy,
    });

    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            sweeper.rooms.sweep(now_ms());
        }
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/r/{room}", get(page))
        .route("/r/{room}/events", get(events))
        .route("/r/{room}/messages", axum::routing::post(send))
        .route("/r/{room}/stream", axum::routing::put(set_stream))
        // A preflight has to be answered for every one of the above, and
        // `options` on each route would be four copies of one answer.
        .fallback(fallback)
        .layer(axum::middleware::from_fn(cors))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("could not listen on {bind}: {e}"))?;
    let local = listener
        .local_addr()
        .map_err(|e| format!("could not read the listening address: {e}"))?;
    println!("pubsplash-chat listening on http://{local}");
    println!("  a room:   http://{local}/r/<name>");
    println!("  its key:  pubsplash-chat key <name>");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        println!("pubsplash-chat: shutting down");
    })
    .await
    .map_err(|e| format!("server stopped: {e}"))
}

fn usage() -> String {
    format!(
        "pubsplash-chat {version} -- drop-in chat for an Icecast broadcast.

USAGE
  pubsplash-chat [options]        start the server
  pubsplash-chat key <room>       print a room's host key, then exit

OPTIONS
  --bind ADDR[:PORT]    where to listen (default 0.0.0.0:8080)
  --brand NAME          name shown on the page (default \"Chat\")
  --lang CODE           fallback page language, en or es (default en). A
                        listener's own browser setting wins over it, and
                        ?lang=es on the URL wins over both
  --secret FILE-LESS    the server secret, inline (or PUBSPLASH_CHAT_SECRET)
  --secret-file PATH    where the secret lives, generated on first run
                        (default ./pubsplash-chat.secret)
  --trust-proxy         read the client address from X-Forwarded-For.
                        ONLY behind a proxy that sets it -- otherwise anybody
                        can mint a fresh spam budget per message
  --rate N              accepted messages per window, per sender (default {rate})
  --rate-window SECONDS the window they are counted over (default {window})
  -h, --help            this
  -V, --version         version

The host key is HMAC(secret, room), so nothing is stored per room and one
server handles any number of them. Change the secret to revoke every key.",
        version = env!("CARGO_PKG_VERSION"),
        rate = RATE,
        window = WINDOW_MS / 1000,
    )
}

/// Reads the server secret, generating and saving one on first run.
///
/// Generating a fresh secret on every start would be much simpler and would
/// silently invalidate every host key the operator has handed out, once per
/// restart -- a failure that looks like "Pubsplash stopped recognising me"
/// rather than like anything to do with this file.
fn load_secret(inline: Option<&str>, path: &str) -> Result<Secret, String> {
    if let Some(value) = inline {
        let value = value.trim();
        if value.is_empty() {
            return Err("the secret is empty".into());
        }
        return Ok(Secret::new(value.as_bytes().to_vec()));
    }
    match std::fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => {
            warn_if_readable(path);
            Ok(Secret::new(bytes))
        }
        _ => {
            let mut bytes = [0u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            let encoded = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            write_secret(path, &encoded)
                .map_err(|e| format!("could not write the secret to {path}: {e}"))?;
            println!("pubsplash-chat: wrote a new server secret to {path}");
            Ok(Secret::new(encoded.into_bytes()))
        }
    }
}

/// Writes the secret so that only its owner can read it.
///
/// This file is the host identity of **every room on the deployment** -- anyone
/// holding it can derive every host key and post as any broadcaster. A default
/// `fs::write` leaves it 0644, so on a shared box any local account could read
/// it; found in review of the first real deployment.
///
/// The mode is set at *creation*, not afterwards, so there is no window in
/// which the file exists and is world-readable.
fn write_secret(path: &str, encoded: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(encoded.as_bytes())?;
        // `mode` above applies only when the file is created, and this path is
        // also reached for an existing-but-empty file. Setting it again costs
        // nothing and covers that case.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    // Windows has no mode bits; the file inherits the directory's ACL, which is
    // the platform's own answer to the same question.
    #[cfg(not(unix))]
    {
        std::fs::write(path, encoded)
    }
}

/// Says so, once, when the secret on disk is readable by anyone else.
///
/// A deployment made before `write_secret` existed still has a 0644 file, and
/// nothing about it looks wrong from the outside -- so the server is the only
/// thing in a position to point it out. A warning rather than a refusal: taking
/// a running station's chat offline over a file mode would be the worse failure.
#[cfg(unix)]
fn warn_if_readable(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode() & 0o077;
    if mode != 0 {
        eprintln!(
            "pubsplash-chat: warning: {path} is readable by other users \
             (mode {:o}). Anyone who can read it can post as the host in every \
             room. Fix with: chmod 600 {path}",
            metadata.permissions().mode() & 0o777
        );
    }
}

#[cfg(not(unix))]
fn warn_if_readable(_path: &str) {}

/// Permissive CORS, and the preflight answer.
///
/// `*` with no credentials is the right setting for this API: there is no
/// cookie and no session to ride, the only privileged action needs a bearer
/// token the browser never holds by default, and allowing any origin is what
/// lets a station embed its own chat in its existing website without the
/// operator editing a config file.
async fn cors(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let preflight = request.method() == Method::OPTIONS;
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, authorization"),
    );
    response
}

async fn fallback() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(INDEX_PAGE.replace("__BRAND__", &escape(&state.brand)))
}

async fn healthz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (rooms, listeners) = state.rooms.census();
    axum::Json(serde_json::json!({ "ok": true, "rooms": rooms, "listeners": listeners }))
}

async fn page(State(state): State<Arc<AppState>>, Path(room): Path<String>) -> Response {
    let Some(room) = clean_room(&room) else {
        return (StatusCode::NOT_FOUND, "No such room").into_response();
    };
    // Only the two identifying strings are substituted; everything else the
    // page needs arrives on the feed's `hello` event, so the markup below does
    // not have to be kept in step with the server's settings.
    Html(
        CHAT_PAGE
            .replace("__ROOM__", &escape(&room))
            .replace("__BRAND__", &escape(&state.brand))
            .replace("__LANG__", &escape(&state.lang)),
    )
    .into_response()
}

/// Escapes text going into the HTML the server assembles.
///
/// Only the room name and the brand reach this: chat text is never templated
/// server-side, it is sent as JSON and written to the DOM with `textContent`.
/// `clean_room` already restricts the room name to an alphanumeric alphabet, so
/// this is the belt to that braces -- and the brand comes off a command line,
/// which is exactly the kind of "trusted" input that stops being trusted the
/// moment somebody generates it from a config file.
fn escape(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// The feed. Replays history, then follows the room.
async fn events(
    State(state): State<Arc<AppState>>,
    Path(room): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let Some(room) = clean_room(&room) else {
        return Err((StatusCode::NOT_FOUND, "No such room").into_response());
    };
    let (listener, history, stream, listeners) = state.rooms.join(&room);

    // Everything a client needs to configure itself, so the page ships no
    // copy of the server's limits and cannot disagree with them.
    let hello = serde_json::json!({
        "room": room,
        "brand": state.brand,
        "listeners": listeners,
        "maxChars": message::TEXT_MAX,
        "stream": stream,
        "limit": { "messages": state.rate, "windowMs": state.window_ms },
        "history": history,
    });
    let hello = Event::default()
        .event("hello")
        .data(hello.to_string());

    // The `Listener` guard lives in the stream's state, so the room's count
    // drops exactly when the client's connection does -- no heartbeat, no
    // reaper. `unfold` rather than `async_stream` to keep the dependency list
    // to what the protocol actually needs.
    struct Feeding {
        listener: room::Listener,
        pending: Option<Event>,
    }
    let stream = futures_util::stream::unfold(
        Feeding {
            listener,
            pending: Some(hello),
        },
        |mut state| async move {
            if let Some(event) = state.pending.take() {
                return Some((Ok(event), state));
            }
            loop {
                match state.listener.rx.recv().await {
                    Ok(Feed::Chat(message)) => {
                        let data = serde_json::to_string(&message).unwrap_or_default();
                        return Some((Ok(Event::default().event("chat").data(data)), state));
                    }
                    Ok(Feed::Stream(stream)) => {
                        let data = serde_json::json!({ "stream": stream }).to_string();
                        return Some((Ok(Event::default().event("stream").data(data)), state));
                    }
                    Ok(Feed::Listeners(count)) => {
                        let data = serde_json::json!({ "count": count }).to_string();
                        return Some((Ok(Event::default().event("listeners").data(data)), state));
                    }
                    // A listener whose phone slept through a busy minute. Tell
                    // them what they missed and keep the connection: dropping
                    // it would turn a gap into an outage, and the page would
                    // reconnect into the same backlog it just fell behind on.
                    Err(RecvError::Lagged(missed)) => {
                        let data = serde_json::json!({ "missed": missed }).to_string();
                        return Some((Ok(Event::default().event("lagged").data(data)), state));
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    );

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEPALIVE).text("keepalive")))
}

/// Accepts one message.
async fn send(
    State(state): State<Arc<AppState>>,
    Path(room): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<axum::Json<Post>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(room) = clean_room(&room) else {
        return reject(Reject::BadRoom, None);
    };
    let Ok(axum::Json(post)) = body else {
        return reject(Reject::Empty, None);
    };

    // A key that is present and wrong is refused outright rather than demoted
    // to an ordinary message. The broadcaster whose key has gone stale needs to
    // be told; silently posting their now-playing line as "Someone" would look
    // like it worked.
    let presented = hostkey::bearer(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    );
    let host = match presented {
        Some(token) if state.secret.verifies(&room, token) => true,
        Some(_) => return reject(Reject::BadHostKey, None),
        None => false,
    };

    let text = match clean_text(&post.text) {
        Ok(text) => text,
        Err(why) => return reject(why, None),
    };

    // Two keys, for two different questions. The ADDRESS is what a flood is
    // counted against, because it is the one part a client cannot choose. The
    // IDENTITY -- that address plus the browser's own token -- is what a
    // nickname is held by, because keying a name on the address alone would
    // give one household, or a whole mobile carrier's CGNAT range, a single
    // name between them.
    let address = client_key(&headers, peer, state.trust_proxy);
    let identity = identity_key(&address, &clean_client(&post.client));

    // The name is settled before the budget is touched. A message refused over
    // its nickname must not also cost the sender one of their five, because the
    // fix is to send it again under the name they already hold.
    let nick = match state
        .rooms
        .resolve_nick(&room, &identity, &clean_nick(&post.nick), host, now_ms())
    {
        Ok(nick) => nick,
        // The wait is only meaningful for a name the sender is holding
        // themselves. A name somebody else has is not one waiting will get
        // them, and offering a countdown there would read as a promise.
        Err((Reject::NameLocked, retry_after_ms)) => {
            return reject(Reject::NameLocked, Some(retry_after_ms));
        }
        Err((why, _)) => return reject(why, None),
    };

    if let Verdict::Blocked { retry_after_ms } =
        state.rooms.check_rate(&identity, &address, host, now_ms())
    {
        return reject(Reject::RateLimited, Some(retry_after_ms));
    }

    let message = state.rooms.publish(
        &room,
        Message {
            id: String::new(), // assigned by `publish`
            nick,
            text,
            ts: now_ms(),
            host,
            system: false,
        },
    );
    axum::Json(serde_json::json!({ "ok": true, "message": message })).into_response()
}

/// Publishes where this room's show can be heard, or takes it down.
///
/// Host key required, and that is the whole access rule: it is the one piece of
/// room state a listener must not be able to write, since it decides what every
/// page in the room connects an `<audio>` element to.
///
/// An empty or missing URL clears it, which is what the broadcaster's app sends
/// when the broadcast ends -- a play button that outlives the show is worse than
/// no play button, because it plays nothing and says nothing about why.
async fn set_stream(
    State(state): State<Arc<AppState>>,
    Path(room): Path<String>,
    headers: HeaderMap,
    body: Result<axum::Json<StreamInfo>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(room) = clean_room(&room) else {
        return reject(Reject::BadRoom, None);
    };
    let presented = hostkey::bearer(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    );
    match presented {
        Some(token) if state.secret.verifies(&room, token) => {}
        _ => return reject(Reject::BadHostKey, None),
    }

    let stream = match body {
        Ok(axum::Json(stream)) => match clean_stream(&stream.url) {
            Some(url) => Some(StreamInfo {
                url,
                // The name is displayed, never resolved, so it goes through the
                // same cleaning a nickname does.
                name: message::clean_nick(&stream.name),
            }),
            // A URL we would not hand to an `<audio>` element clears the
            // player rather than being stored and quietly never working.
            None => None,
        },
        Err(_) => None,
    };
    state.rooms.set_stream(&room, stream.clone());
    axum::Json(serde_json::json!({ "ok": true, "stream": stream })).into_response()
}

fn reject(why: Reject, retry_after_ms: Option<u64>) -> Response {
    let status = StatusCode::from_u16(why.status()).unwrap_or(StatusCode::BAD_REQUEST);
    let mut body = serde_json::json!({ "ok": false, "error": why.code() });
    if let Some(ms) = retry_after_ms {
        body["retryAfterMs"] = serde_json::json!(ms);
    }
    (status, axum::Json(body)).into_response()
}

/// What the rate limiter counts against.
///
/// The socket's address by default. With `--trust-proxy`, the *first* entry of
/// `X-Forwarded-For` -- the client as the nearest proxy saw it. A client can
/// prepend anything it likes to that header, which is precisely why this is
/// opt-in: unproxied, believing it would hand every flooder an unlimited supply
/// of fresh identities.
///
/// The port is dropped, so a browser opening a second connection is the same
/// sender, and so is a second tab.
fn client_key(headers: &HeaderMap, peer: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return forwarded.to_string();
        }
    }
    peer.ip().to_string()
}

/// Who a nickname is held by: the address, plus the browser's own token when
/// it sent one. Falls back to the address alone, which is what a client that
/// sends no token gets and is still correct for a single-user connection.
fn identity_key(address: &str, client: &str) -> String {
    if client.is_empty() {
        address.to_string()
    } else {
        format!("{address}|{client}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn two_people_behind_one_address_are_two_identities() {
        // The household case, and the CGNAT case: one address, two browsers, so
        // each may hold a name of their own.
        assert_ne!(
            identity_key("198.51.100.7", "aaa"),
            identity_key("198.51.100.7", "bbb")
        );
        // A client that sends no token is simply its address.
        assert_eq!(identity_key("198.51.100.7", ""), "198.51.100.7");
        // And a token cannot be carried to another address.
        assert_ne!(
            identity_key("198.51.100.7", "aaa"),
            identity_key("203.0.113.9", "aaa")
        );
    }

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)), 51234)
    }

    #[test]
    fn the_rate_key_ignores_the_port_so_a_second_tab_shares_the_budget() {
        let first = client_key(&HeaderMap::new(), peer(), false);
        let mut second_peer = peer();
        second_peer.set_port(51235);
        assert_eq!(first, client_key(&HeaderMap::new(), second_peer, false));
    }

    #[test]
    fn a_forwarded_header_is_ignored_unless_it_is_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
        // This is the flooding hole: believing the header unproxied would let a
        // client pick a new identity, and so a new budget, for every message.
        assert_eq!(client_key(&headers, peer(), false), "198.51.100.7");
        assert_eq!(client_key(&headers, peer(), true), "203.0.113.9");
    }

    #[test]
    fn a_forwarded_chain_credits_the_original_client() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 10.0.0.1, 10.0.0.2"),
        );
        assert_eq!(client_key(&headers, peer(), true), "203.0.113.9");
    }

    #[test]
    fn trusting_the_proxy_still_falls_back_when_it_sets_nothing() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("  "));
        assert_eq!(client_key(&headers, peer(), true), "198.51.100.7");
        assert_eq!(client_key(&HeaderMap::new(), peer(), true), "198.51.100.7");
    }

    #[test]
    fn the_pages_carry_the_placeholders_the_handlers_replace() {
        // If a page is edited and a placeholder is lost, the room name silently
        // stops reaching the client -- which is a blank page, not a build error.
        assert!(CHAT_PAGE.contains("__ROOM__"), "chat.html lost __ROOM__");
        assert!(CHAT_PAGE.contains("__BRAND__"), "chat.html lost __BRAND__");
        assert!(CHAT_PAGE.contains("__LANG__"), "chat.html lost __LANG__");
        assert!(INDEX_PAGE.contains("__BRAND__"), "index.html lost __BRAND__");
    }

    #[test]
    fn templated_text_cannot_close_a_tag() {
        assert_eq!(
            escape("<script>alert('x')</script>"),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"
        );
        assert_eq!(escape("Bob & \"Jim\""), "Bob &amp; &quot;Jim&quot;");
    }
}
