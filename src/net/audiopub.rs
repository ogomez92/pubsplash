//! Audio Pub web API client: login, stream key retrieval, stream lifecycle,
//! and chat. Sessions are a `token` JWT cookie set by `POST /login`.

use crate::secret::Secret;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("invalid email or password")]
    BadCredentials,
    #[error("unexpected server response: {0}")]
    Unexpected(String),
}

/// What [`AudioPubClient::open_events`] found at the live-events endpoint.
///
/// The server answers a stream that is finished — or that never existed — with
/// **`204 No Content`, not a 404** (`src/routes/live/[id]/events/+server.ts` in
/// the audiopub-sv source). `error_for_status` treats 204 as success, so this
/// case arrives as a perfectly good response whose body ends immediately, and
/// is otherwise indistinguishable from a feed that dropped the instant it
/// opened. Telling the two apart is the difference between "retry" and "this
/// stream is over"; hence the enum.
pub enum EventsStream {
    /// A live feed. The caller pumps the byte stream through an `SseParser`.
    Open(reqwest::Response),
    /// No live stream under this id any more.
    Gone,
}

#[derive(Debug, Clone)]
pub struct StreamIdentity {
    /// Audio Pub user id — also the Icecast mount name.
    pub user_id: String,
    /// The Icecast source password.
    pub stream_key: Secret,
}

/// Matches `mastodon::net` and `tts::net`, the app's other HTTP clients.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Shorter, because ending a stream runs on the app-exit path: `NetHandle::drop`
/// joins the network thread, so a wedged request here holds up the whole
/// shutdown with no window on screen to explain the wait.
const END_STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// The page that tells a broadcaster how to reach this instance. Its
/// `__data.json` carries the account's stream key; the page itself carries the
/// endpoint the key is meant to be used against.
const INSTRUCTIONS_PATH: &str = "/stream/instructions";

/// The one-line source URL the instructions page offers, and the anchor
/// [`parse_published_endpoint`] reads the endpoint out of.
const SOURCE_URL_SCHEME: &str = "icecast://";

pub struct AudioPubClient {
    http: reqwest::Client,
    /// Site base URL, no trailing slash, e.g. `https://audiopub.site`.
    base: String,
}

impl AudioPubClient {
    pub fn new(site_url: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("pubsplash/", env!("CARGO_PKG_VERSION")))
            // Connect only. A client-wide `.timeout()` would cover the whole
            // response including a streamed body, which would cut the endless
            // live-events feed in `open_events` -- so the finite calls carry
            // their own per-request timeouts instead.
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            base: site_url.trim_end_matches('/').to_string(),
        })
    }

    /// Logs in. The `token` cookie is set on success.
    ///
    /// SvelteKit form actions answer differently depending on the Accept
    /// header: browsers get a native 303 redirect, everyone else gets a 200
    /// JSON envelope (`{"type":"redirect",...}` or `{"type":"failure",
    /// "status":401,...}`). Handle both.
    pub async fn login(&self, email: &str, password: &str) -> Result<(), ApiError> {
        let response = self
            .http
            .post(format!("{}/login", self.base))
            .timeout(REQUEST_TIMEOUT)
            .form(&[("email", email), ("password", password)])
            .send()
            .await?;
        match action_result(response).await? {
            ActionResult::Redirect(_) => Ok(()),
            ActionResult::Failure { status: 401, .. } => Err(ApiError::BadCredentials),
            ActionResult::Failure { status, message } => Err(ApiError::Unexpected(format!(
                "login failed ({status}): {}",
                message.unwrap_or_default()
            ))),
        }
    }

    /// Fetches the user id and stream key from the stream-instructions page
    /// data endpoint (which also generates a key if the account lacks one).
    pub async fn stream_identity(&self) -> Result<StreamIdentity, ApiError> {
        let response = self
            .http
            .get(format!("{}{INSTRUCTIONS_PATH}/__data.json", self.base))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        let body: Value = response.json().await?;
        parse_stream_identity(&body)
            .ok_or_else(|| ApiError::Unexpected("no user in stream instructions data".into()))
    }

    /// Creates a stream and returns its id. If a non-finished stream already
    /// exists, the server redirects to it and we reuse that id.
    pub async fn create_stream(
        &self,
        title: &str,
        description: &str,
        archive: bool,
    ) -> Result<String, ApiError> {
        let mut form = vec![("title", title), ("description", description)];
        if archive {
            // Checkbox field: the server checks `=== "on"`; omit when unchecked.
            form.push(("shouldArchive", "on"));
        }
        let response = self
            .http
            .post(format!("{}/live/new", self.base))
            .timeout(REQUEST_TIMEOUT)
            .form(&form)
            .send()
            .await?;
        match action_result(response).await? {
            ActionResult::Redirect(location) => {
                if let Some(id) = stream_id_from_location(&location) {
                    Ok(id)
                } else {
                    Err(ApiError::Unexpected(format!(
                        "create stream redirected to {location}"
                    )))
                }
            }
            ActionResult::Failure { status, message } => Err(ApiError::Unexpected(format!(
                "create stream failed ({status}): {}",
                message.unwrap_or_default()
            ))),
        }
    }

    /// Sends a chat message to the stream; returns the created message JSON.
    pub async fn send_chat(&self, stream_id: &str, content: &str) -> Result<Value, ApiError> {
        let response = self
            .http
            .post(format!("{}/live/{stream_id}", self.base))
            .timeout(REQUEST_TIMEOUT)
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }

    /// Ends the stream.
    pub async fn end_stream(&self, stream_id: &str) -> Result<(), ApiError> {
        self.http
            .delete(format!("{}/live/{stream_id}", self.base))
            .timeout(END_STREAM_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Opens the live-events SSE response for a stream. The caller pumps the
    /// byte stream through an `SseParser`.
    pub async fn open_events(&self, stream_id: &str) -> Result<EventsStream, ApiError> {
        let response = self
            .http
            .get(format!("{}/live/{stream_id}/events", self.base))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await?
            .error_for_status()?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(EventsStream::Gone);
        }
        Ok(EventsStream::Open(response))
    }

    /// Asks the instance where its broadcasters are supposed to publish.
    ///
    /// There is no API for this and no setting that carries it. Upstream's
    /// `src/routes/stream/instructions/+page.svelte` has `live.audiopub.site`
    /// and `8000` written into the template as literals, so an instance that
    /// publishes anywhere else is one whose operator edited that file — and the
    /// page it renders is the only place the answer exists. `.env.example`'s
    /// `ICECAST_HOST` is the *server's* own admin address (`127.0.0.1:8000`),
    /// not the one a broadcaster dials, so it is no help even where it leaks.
    ///
    /// What is read out of the page is the one-line source URL it offers to
    /// software that wants everything in a single box:
    /// `icecast://source:<key>@host:port/<user-id>`. That anchor is deliberate.
    /// A fork is free to translate every label around it — audio.gomsen.com's
    /// page is entirely in Spanish, where "Server address" reads "Dirección del
    /// servidor" — but the scheme, the `@` and the `:` are structure rather than
    /// prose and survive translation. The URL is also rendered logged out, with
    /// the key and mount left as placeholders and the host and port real, so
    /// this needs no session and can run before the login.
    ///
    /// A failure of any kind is the caller's cue to keep whatever it already
    /// had. Nothing here may stand between the user and their broadcast.
    pub async fn published_endpoint(&self) -> Result<Option<(String, u16)>, ApiError> {
        let body = self
            .http
            .get(format!("{}{INSTRUCTIONS_PATH}", self.base))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_published_endpoint(&body))
    }

    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base
    }
}

/// Pulls the publishing host and port out of a rendered instructions page.
///
/// Every `icecast://` on the page is tried in turn, because the masked and the
/// revealed form of the URL both appear and a fork may put more prose around
/// them; the first that parses to something that could be an address wins.
/// [`super::icecast::split_host_port`] does the parsing, so an IPv6 literal,
/// the embedded credentials and the trailing mount are all handled exactly as
/// they are for a hand-typed field.
///
/// The placeholder guard is what stops a page whose values were never filled in
/// from being read as an address: `&lt;` is how Svelte escapes the `<` of
/// `<your-server>`, and no real host contains that or a `>`.
fn parse_published_endpoint(page: &str) -> Option<(String, u16)> {
    page.match_indices(SOURCE_URL_SCHEME)
        .find_map(|(start, _)| {
            let rest = &page[start..];
            // The URL runs until the markup or the sentence around it resumes. A
            // literal `<` can only be the next tag, since the page's own
            // placeholders arrive escaped.
            let end = rest
                .find(|c: char| c == '<' || c == '"' || c == '\'' || c.is_whitespace())
                .unwrap_or(rest.len());
            let (host, port) = super::icecast::split_host_port(&rest[..end]).ok()?;
            if host.contains("&lt;") || host.contains('>') {
                return None;
            }
            Some((host, port.unwrap_or(super::icecast::DEFAULT_PORT)))
        })
}

/// The outcome of a SvelteKit form action, normalized across the native
/// redirect flow and the JSON envelope flow.
enum ActionResult {
    Redirect(String),
    Failure {
        status: u16,
        message: Option<String>,
    },
}

/// Interprets a form-action response. Native flow: 3xx with a Location
/// header. JSON flow: 200 with `{"type": "redirect"|"failure"|"error", ...}`
/// where `data` is a devalue-encoded flat array.
async fn action_result(response: reqwest::Response) -> Result<ActionResult, ApiError> {
    let status = response.status();
    if status.is_redirection() {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("/")
            .to_string();
        return Ok(ActionResult::Redirect(location));
    }
    if !status.is_success() {
        return Err(ApiError::Unexpected(format!("action returned {status}")));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| ApiError::Unexpected(format!("action returned non-JSON body: {e}")))?;
    parse_action_envelope(&body)
}

fn parse_action_envelope(body: &Value) -> Result<ActionResult, ApiError> {
    match body.get("type").and_then(Value::as_str) {
        Some("redirect") => Ok(ActionResult::Redirect(
            body.get("location")
                .and_then(Value::as_str)
                .unwrap_or("/")
                .to_string(),
        )),
        Some("success") => Ok(ActionResult::Redirect(String::new())),
        Some("failure") | Some("error") => {
            let status = body.get("status").and_then(Value::as_u64).unwrap_or(400) as u16;
            // `data` is a devalue string: a JSON array whose first element
            // maps field names to indices of the values.
            let message = body
                .get("data")
                .and_then(Value::as_str)
                .and_then(|data| serde_json::from_str::<Vec<Value>>(data).ok())
                .and_then(|data| {
                    let index = data.first()?.get("message")?.as_u64()? as usize;
                    Some(data.get(index)?.as_str()?.to_string())
                })
                .or_else(|| {
                    body.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            Ok(ActionResult::Failure { status, message })
        }
        other => Err(ApiError::Unexpected(format!(
            "unrecognized action response type {other:?}"
        ))),
    }
}

fn stream_id_from_location(location: &str) -> Option<String> {
    let id = location.trim_end_matches('/').rsplit('/').next()?;
    if location.contains("/live/") && !id.is_empty() && id != "new" {
        Some(id.to_string())
    } else {
        None
    }
}

/// Resolves a value in SvelteKit's devalue flat-array encoding: objects map
/// keys to indices into the same array, so following a path means repeated
/// index lookups.
fn devalue_get<'a>(data: &'a [Value], index: usize, path: &[&str]) -> Option<&'a Value> {
    let mut current = data.get(index)?;
    for key in path {
        let idx = current.as_object()?.get(*key)?.as_u64()? as usize;
        current = data.get(idx)?;
    }
    Some(current)
}

/// Extracts `{user: {id, streamKey}}` from a `__data.json` payload.
///
/// Several nodes may carry a `user` object (the root layout returns the
/// session user without a stream key); only the stream-instructions page
/// node has both `id` and `streamKey`, so skip nodes that don't match.
fn parse_stream_identity(body: &Value) -> Option<StreamIdentity> {
    let nodes = body.get("nodes")?.as_array()?;
    nodes.iter().find_map(|node| {
        let data = node.get("data")?.as_array()?;
        let user = devalue_get(data, 0, &["user"]).filter(|u| u.is_object())?;
        // `user` is itself an object of key -> index.
        let id_idx = user.get("id")?.as_u64()? as usize;
        let key_idx = user.get("streamKey")?.as_u64()? as usize;
        Some(StreamIdentity {
            user_id: data.get(id_idx)?.as_str()?.to_string(),
            stream_key: Secret::new(data.get(key_idx)?.as_str()?),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devalue_stream_identity() {
        // Real shape: the layout node carries the session user (no
        // streamKey) before the page node that has both fields. The parser
        // must skip past the layout node rather than give up.
        let body: Value = serde_json::from_str(
            r#"{
                "type": "data",
                "nodes": [
                    {
                        "type": "data",
                        "data": [
                            {"user": 1, "isAdmin": 4},
                            {"id": 2, "name": 3},
                            "user-uuid-1",
                            "somebody",
                            false
                        ],
                        "uses": {}
                    },
                    {
                        "type": "data",
                        "data": [
                            {"user": 1},
                            {"id": 2, "streamKey": 3},
                            "user-uuid-1",
                            "key-uuid-9"
                        ],
                        "uses": {}
                    }
                ]
            }"#,
        )
        .unwrap();
        let identity = parse_stream_identity(&body).unwrap();
        assert_eq!(identity.user_id, "user-uuid-1");
        assert_eq!(identity.stream_key.as_str(), "key-uuid-9");
    }

    /// Upstream's own page, logged out: the host and port are real even though
    /// the key and the mount are placeholders, which is what lets the endpoint
    /// be read before anyone has signed in.
    #[test]
    fn reads_the_endpoint_off_the_stock_instructions_page() {
        let page = r#"
            <p class="url-display">
                <code class="sensitive masked full-url">
                icecast://source:&lt;your-stream-key>@live.audiopub.site:8000/&lt;your-user-id>
            </code>
            </p>
        "#;
        assert_eq!(
            parse_published_endpoint(page),
            Some(("live.audiopub.site".to_string(), 8000))
        );
    }

    /// audio.gomsen.com, which is why any of this exists: a fork on a port that
    /// is not 8000, with every label around the URL translated. The prose moves
    /// and the anchor does not.
    #[test]
    fn reads_a_translated_forks_endpoint_on_its_own_port() {
        let page = r#"
            <dt>Dirección del servidor</dt>
            <dd><code>live.audio.gomsen.com</code></dd>
            <dt>Puerto</dt>
            <dd><code>8010</code></dd>
            <p class="url-display">
                <code class="sensitive masked full-url">
                icecast://source:&lt;tu-clave-de-emisión>@live.audio.gomsen.com:8010/&lt;tu-id-de-usuario>
            </code>
            </p>
        "#;
        assert_eq!(
            parse_published_endpoint(page),
            Some(("live.audio.gomsen.com".to_string(), 8010))
        );
    }

    /// The same page for someone signed in with "show sensitive information"
    /// ticked, where the placeholders are gone and the key is a real UUID.
    #[test]
    fn reads_the_endpoint_out_of_a_revealed_url() {
        let page = r#"<code class="sensitive full-url">icecast://source:0f7a1c2e-0000-4d00-9000-1234567890ab@live.example.org:8100/8b3d5f60-1111-4a00-8000-abcdefabcdef</code>"#;
        assert_eq!(
            parse_published_endpoint(page),
            Some(("live.example.org".to_string(), 8100))
        );
    }

    /// A URL with no port on it means Icecast's own default, the same way it
    /// does everywhere else the app parses one.
    #[test]
    fn a_portless_url_means_the_icecast_default() {
        let page = "icecast://source:key@stream.example.org/mount";
        assert_eq!(
            parse_published_endpoint(page),
            Some((
                "stream.example.org".to_string(),
                super::super::icecast::DEFAULT_PORT
            ))
        );
    }

    /// A fork that left the host as a placeholder has told us nothing, and
    /// "&lt;your-server>" must never be dialled. The scan carries on past it to
    /// the URL that does name a host.
    #[test]
    fn placeholder_hosts_are_skipped_not_dialled() {
        let unfilled = "icecast://source:&lt;key>@&lt;your-server>:8000/&lt;your-user-id>";
        assert_eq!(parse_published_endpoint(unfilled), None);
        let page = format!("{unfilled} <p>icecast://source:k@live.example.net:8020/m</p>");
        assert_eq!(
            parse_published_endpoint(&page),
            Some(("live.example.net".to_string(), 8020))
        );
    }

    /// A page with nothing to read — a fork that rewrote the instructions, or a
    /// login wall — leaves the caller on whatever it already had.
    #[test]
    fn a_page_without_a_source_url_answers_nothing() {
        assert_eq!(parse_published_endpoint("<h1>How to stream</h1>"), None);
        assert_eq!(parse_published_endpoint(""), None);
    }

    /// Hits the real audiopub.site. Run explicitly with
    /// `cargo test real_login -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_login_bad_credentials() {
        let client = AudioPubClient::new("https://audiopub.site/").unwrap();
        match client.login("probe@example.com", "wrongpassword").await {
            Err(ApiError::BadCredentials) => {}
            other => panic!("expected BadCredentials, got {other:?}"),
        }
    }

    /// The whole custom-instance path against a real server: discover the
    /// endpoint, log in, and fetch the stream key — everything Connect does,
    /// and nothing that creates a stream or puts audio on the air.
    ///
    /// Driven by `PUBSPLASH_TEST_SITE`, `PUBSPLASH_TEST_EMAIL` and
    /// `PUBSPLASH_TEST_PASSWORD`, and skipped when they are unset, so it can
    /// live in the tree without a credential in it. Run with
    /// `cargo test real_instance_connect -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn real_instance_connect() {
        let (Ok(site), Ok(email), Ok(password)) = (
            std::env::var("PUBSPLASH_TEST_SITE"),
            std::env::var("PUBSPLASH_TEST_EMAIL"),
            std::env::var("PUBSPLASH_TEST_PASSWORD"),
        ) else {
            eprintln!("skipped: set PUBSPLASH_TEST_SITE/_EMAIL/_PASSWORD to run this");
            return;
        };

        let client = AudioPubClient::new(&site).unwrap();

        let endpoint = client
            .published_endpoint()
            .await
            .unwrap_or_else(|e| panic!("reading {site}'s instructions page: {e}"));
        let (host, port) = endpoint.expect("the instance should name an endpoint");
        eprintln!("endpoint: {host}:{port}");

        client
            .login(&email, &password)
            .await
            .unwrap_or_else(|e| panic!("logging in to {site}: {e}"));
        eprintln!("login: ok");

        let identity = client
            .stream_identity()
            .await
            .unwrap_or_else(|e| panic!("fetching the stream key from {site}: {e}"));
        assert!(!identity.user_id.is_empty(), "the mount must not be empty");
        assert!(
            !identity.stream_key.is_empty(),
            "the source password must not be empty"
        );
        // The mount is the user id; the key is a password and stays out of the
        // output beyond enough to tell one from another.
        eprintln!(
            "mount: {} / key: {}… ({} chars)",
            identity.user_id,
            &identity.stream_key.as_str()[..8],
            identity.stream_key.as_str().len()
        );
    }

    /// Hits two real instances, and is the regression test for the whole of
    /// [`parse_published_endpoint`]: the markup it reads belongs to somebody
    /// else and can be rewritten without warning, so the only honest check is
    /// against the live pages. audio.gomsen.com is here because it is the case
    /// the guess gets wrong — its port is 8010, and `live.audio.gomsen.com:8000`
    /// is a different streaming server altogether. Run with
    /// `cargo test real_published_endpoint -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_published_endpoints() {
        for (site, expected) in [
            ("https://audiopub.site/", ("live.audiopub.site", 8000)),
            ("https://audio.gomsen.com/", ("live.audio.gomsen.com", 8010)),
        ] {
            let client = AudioPubClient::new(site).unwrap();
            let found = client
                .published_endpoint()
                .await
                .unwrap_or_else(|e| panic!("{site} instructions page: {e}"));
            assert_eq!(
                found,
                Some((expected.0.to_string(), expected.1)),
                "{site} should publish to {}:{}",
                expected.0,
                expected.1
            );
        }
    }

    #[test]
    fn failure_envelope_yields_message() {
        let body: Value = serde_json::from_str(
            r#"{"type":"failure","status":401,"data":"[{\"email\":1,\"message\":2},\"probe@example.com\",\"Invalid email or password\"]"}"#,
        )
        .unwrap();
        match parse_action_envelope(&body).unwrap() {
            ActionResult::Failure { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message.as_deref(), Some("Invalid email or password"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn redirect_envelope_yields_location() {
        let body: Value =
            serde_json::from_str(r#"{"type":"redirect","status":303,"location":"/live/abc-123"}"#)
                .unwrap();
        match parse_action_envelope(&body).unwrap() {
            ActionResult::Redirect(location) => {
                assert_eq!(
                    stream_id_from_location(&location).as_deref(),
                    Some("abc-123")
                );
            }
            _ => panic!("expected redirect"),
        }
    }

    /// Answers one request with `status` and closes, returning the bound port.
    fn one_shot_server(status: &'static str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut scratch = [0u8; 1024];
            let _ = socket.read(&mut scratch);
            let _ = socket
                .write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n").as_bytes());
        });
        port
    }

    /// The server answers a finished or unknown stream with **204, not 404**
    /// (`src/routes/live/[id]/events/+server.ts` in audiopub-sv). 204 is a
    /// success status, so `error_for_status` passes it straight through and it
    /// arrives as a good response whose body ends at once — indistinguishable
    /// from a feed that dropped instantly unless the status is checked. Getting
    /// this wrong means reconnecting forever against a stream that is over.
    #[tokio::test]
    async fn finished_stream_reports_gone_not_a_live_feed() {
        let port = one_shot_server("204 No Content");
        let client = AudioPubClient::new(&format!("http://127.0.0.1:{port}")).unwrap();
        assert!(matches!(
            client.open_events("dead-stream").await.unwrap(),
            EventsStream::Gone
        ));
    }

    #[tokio::test]
    async fn live_stream_reports_an_open_feed() {
        let port = one_shot_server("200 OK\r\nContent-Type: text/event-stream");
        let client = AudioPubClient::new(&format!("http://127.0.0.1:{port}")).unwrap();
        assert!(matches!(
            client.open_events("live-stream").await.unwrap(),
            EventsStream::Open(_)
        ));
    }

    #[test]
    fn missing_user_yields_none() {
        // Logged-out shape: `return {user: null}` — devalue may encode null
        // directly or via index; cover the direct-null variant.
        let body: Value = serde_json::from_str(
            r#"{"type":"data","nodes":[null,{"type":"data","data":[{"user":1},null],"uses":{}}]}"#,
        )
        .unwrap();
        assert!(parse_stream_identity(&body).is_none());
    }
}
