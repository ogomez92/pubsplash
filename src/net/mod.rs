//! Networking: the Audiopub web API, the live-events SSE consumer, and the
//! Icecast source connection. Everything here runs on a tokio runtime living
//! on a background thread; the UI talks to it over channels.

pub mod audiopub;
pub mod icecast;
pub mod rtmp;
pub mod sse;
pub mod stats;
pub mod youtube;

use crate::t;
use crate::secret::Secret;
use audiopub::{AudioPubClient, EventsStream, StreamIdentity};
use icecast::{IcecastConnection, IcecastError, IcecastTarget};
use rtmp::{RtmpError, RtmpProcess, RtmpTarget};
use sse::{LiveEvent, SseParser};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use youtube::ChannelRef;

/// A configured streaming service, snapshotted by the UI before it is sent to
/// the network thread.
#[derive(Debug, Clone)]
pub enum ServiceProfile {
    Audiopub {
        id: String,
        nickname: String,
        site_url: String,
        server: String,
        port: u16,
        email: String,
        password: Secret,
        /// Whether `server` and `port` are still the guess the app derives from
        /// `site_url` rather than something the user chose.
        ///
        /// The guess is `live.<site host>` on port 8000, which is what upstream
        /// publishes and what every instance is *assumed* to publish. When it is
        /// wrong it is wrong silently and expensively — audio.gomsen.com is on
        /// 8010, and `live.audio.gomsen.com:8000` answers, as somebody else's
        /// streaming server — so a still-default endpoint is worth replacing
        /// with the instance's own answer. A typed-in one never is: the fields
        /// exist precisely so an operator can send us somewhere the page does
        /// not name, and second-guessing that would break the override the
        /// moment it was needed. Decided in `ui::service_profile_from`, where
        /// the rest of the pre-flight judgements are made.
        endpoint_is_default: bool,
    },
    Icecast {
        id: String,
        nickname: String,
        server: String,
        port: u16,
        mount: String,
        username: String,
        password: Secret,
        /// Where to count listeners, when they do not listen on the mount we
        /// publish to. Empty counts `mount` itself. See [`stats::stats_target`]
        /// for the forms this accepts.
        listener_url: String,
    },
    /// An RTMP ingest, published to through ffmpeg. YouTube by default; the URL
    /// is editable, so any RTMP server works.
    ///
    /// Everything ffmpeg needs is resolved by the UI *before* this is built —
    /// the binary is located and probed at Connect, where the user is waiting
    /// for an answer — so the network thread never has to decide what to do
    /// about a missing encoder halfway into a broadcast.
    Youtube {
        id: String,
        nickname: String,
        ffmpeg: PathBuf,
        url: String,
        key: Secret,
        image: Option<PathBuf>,
        video_bitrate_kbps: u32,
        h264: &'static str,
        aac: &'static str,
        /// Which broadcast to read chat from. `None` leaves the service with no
        /// chat at all, which is the right answer for an RTMP target that is not
        /// YouTube.
        chat: Option<ChannelRef>,
    },
}

/// Commands from the UI to the network runtime.
pub enum NetCommand {
    Connect {
        profile: ServiceProfile,
    },
    Disconnect,
    StartStream {
        title: String,
        description: String,
        archive: bool,
        content_type: String,
        /// What the audio engine is putting on `audio`, which depends on the
        /// service: MP3 for Icecast (Audio Pub included), and raw interleaved
        /// 16-bit PCM for an RTMP target, whose ffmpeg does its own encoding.
        /// The UI chooses this and the matching `EngineCommand` together; see
        /// `ui::begin_stream`.
        audio: tokio_mpsc::Receiver<Vec<u8>>,
        /// The configured audio bitrate. Already applied to the MP3 encoder by
        /// the time this arrives, and needed here only because an RTMP target's
        /// encoding happens on this side of the channel rather than in the
        /// engine.
        audio_bitrate_kbps: u32,
    },
    StopStream,
    SendChat(String),
    /// Drop the live-events connection and open a new one, leaving the stream
    /// itself alone. The user's escape hatch when chat has gone quiet.
    ReconnectChat,
    Shutdown,
}

/// What the outgoing audio (Icecast source) connection is doing.
///
/// The sibling of [`ChatFeedState`] for the other half of a broadcast, and it
/// follows the same rule for the same reason: only *transitions* are reported,
/// so a four-minute outage puts two or three lines in the log rather than one
/// per attempt.
#[derive(Debug)]
pub enum AudioLinkState {
    /// The source connection dropped; we are reconnecting. Once per outage.
    Interrupted { reason: String },
    /// A reconnect succeeded. The same stream, not a new one.
    Restored { gap_seconds: u64 },
    /// Halfway through the retry budget and still trying, so a long outage is
    /// not silent between its opening line and its last one.
    StillRetrying { remaining_seconds: u64 },
}

/// What the live-events (chat) connection is doing, for the log.
///
/// Only *transitions* are reported — a stream can be gone for an hour, and the
/// user does not need to be told once a minute.
#[derive(Debug)]
pub enum ChatFeedState {
    /// The feed dropped; we are retrying. Sent once per outage.
    Interrupted { reason: String },
    /// A reconnect succeeded after an interruption.
    Restored,
    /// The server has no live stream under this id any more (`204`).
    StreamGone,
    /// The server archived this stream.
    Archived,
}

/// Events from the network runtime to the UI (polled on the UI pump timer).
#[derive(Debug)]
pub enum NetEvent {
    Connected {
        service_id: String,
        display_name: String,
    },
    ConnectFailed {
        message: String,
    },
    Disconnected,
    StreamStarted {
        stream_id: String,
    },
    StreamEnded,
    StreamError {
        message: String,
    },
    Chat(sse::ChatMessage),
    Listeners {
        active: u32,
        peak: u32,
    },
    ChatSent,
    ChatSendFailed {
        message: String,
    },
    ChatFeed(ChatFeedState),
    AudioLink(AudioLinkState),
    /// The server's own view of this stream, verbatim (`pending`, `active`,
    /// `disconnected`, `finished`).
    ///
    /// Arrives over the live-events feed but is *not* a chat fact, which is why
    /// it is its own event: an open Icecast socket proves only that Icecast took
    /// the source, while Audio Pub does not serve a single listener until
    /// `sourceConnected()` has ffprobed the mount — and kills the source if that
    /// probe fails. This is the only signal that tells the two apart.
    ///
    /// Every value is sent, `active` included, and the server re-sends the
    /// current state on every live-events connect. De-duplication is the pump's
    /// job (it holds the last value); doing it here would mean a chat reconnect
    /// silently dropped the one event that says listeners can hear us.
    ServerStreamState { state: String },
}

/// The network thread's end of the event channel.
///
/// Sending also wakes the UI's idle loop, which is what drains these. Doing it
/// here rather than at each call site means a new event can never be added
/// without the doorbell - and a missed doorbell is a message that sits unread
/// until something else happens to wake the loop.
///
/// `wxWakeUpIdle` is thread-safe and carries nothing; the events themselves
/// travel by channel. (It has to be a doorbell: wxdragon's `call_after` needs
/// `Send`, and `App` is an `Rc` of `RefCell`s.)
#[derive(Clone)]
pub struct EventSender(crossbeam_channel::Sender<NetEvent>);

impl EventSender {
    /// The error is boxed because `NetEvent` is large and every send returns
    /// this type, hot path included; only the failure needs to carry the event
    /// back, and no caller looks at it.
    pub fn send(&self, event: NetEvent) -> Result<(), Box<crossbeam_channel::SendError<NetEvent>>> {
        let result = self.0.send(event);
        wxdragon::wake_up_idle();
        result.map_err(Box::new)
    }
}

pub struct NetHandle {
    commands: tokio_mpsc::UnboundedSender<NetCommand>,
    pub events: crossbeam_channel::Receiver<NetEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NetHandle {
    pub fn start() -> Self {
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let thread = std::thread::Builder::new()
            .name("net".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building tokio runtime");
                runtime.block_on(net_loop(cmd_rx, EventSender(event_tx)));
            })
            .expect("spawning net thread");
        Self {
            commands: cmd_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    pub fn send(&self, command: NetCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for NetHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(NetCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum Connection {
    Audiopub {
        /// Shared because the chat feed task reconnects on its own and so needs
        /// the cookie store to outlive any one request.
        client: Arc<AudioPubClient>,
        identity: StreamIdentity,
        server: String,
        port: u16,
    },
    Icecast {
        server: String,
        port: u16,
        mount: String,
        username: String,
        password: Secret,
        listener_url: String,
    },
    Youtube {
        target: RtmpTarget,
        chat: Option<ChannelRef>,
    },
}

fn audiopub_client(connection: &Connection) -> Option<&AudioPubClient> {
    match connection {
        Connection::Audiopub { client, .. } => Some(client),
        Connection::Icecast { .. } | Connection::Youtube { .. } => None,
    }
}

/// What a service calls the thing it sends, for the one message that has to name
/// it. Chat is Audiopub's alone — a direct Icecast mount has no chat channel at
/// all, and YouTube's needs a signed-in account (see [`youtube`]).
fn chat_unavailable(connection: &Connection) -> &'static str {
    match connection {
        Connection::Audiopub { .. } => "chat is available",
        Connection::Icecast { .. } => "chat is only available for Audiopub and YouTube services",
        Connection::Youtube { .. } => {
            "Pubsplash can read YouTube chat but not post to it. Sending needs a signed-in \
             Google account, which YouTube only offers through its quota-limited API"
        }
    }
}

struct ActiveStream {
    stream_id: String,
    sse_task: Option<tokio::task::JoinHandle<()>>,
    /// Polls the Icecast status document for listener counts. Only a direct
    /// Icecast service has one: an Audio Pub stream is told over the same feed
    /// that carries its chat, and asking Icecast as well would be a second,
    /// worse answer to a question already answered.
    stats_task: Option<tokio::task::JoinHandle<()>>,
    icecast_task: tokio::task::JoinHandle<()>,
    /// Rings the chat feed task to abandon what it is doing and reconnect now.
    ///
    /// A channel rather than a `Notify` because delivery has to be durable: a
    /// `Notified` future that is dropped after being notified — which is what
    /// the losing branch of a `select!` does — can swallow the notification,
    /// and a button press that silently does nothing is the worst outcome for
    /// a control whose whole purpose is reassurance. A queued message survives
    /// the dropped future.
    chat_reconnect: tokio_mpsc::UnboundedSender<()>,
}

impl ActiveStream {
    fn abort(&self) {
        if let Some(task) = &self.sse_task {
            task.abort();
        }
        if let Some(task) = &self.stats_task {
            task.abort();
        }
        self.icecast_task.abort();
    }
}

fn normalize_mount(mount: &str) -> String {
    let mount = mount.trim();
    if !mount.is_empty() && mount.chars().all(|c| c == '/') {
        "/".to_string()
    } else {
        mount.trim_start_matches('/').to_string()
    }
}

fn audiopub_target_for(
    server: &str,
    port: u16,
    identity: &StreamIdentity,
    content_type: &str,
) -> Result<IcecastTarget, String> {
    let server = server.trim();
    if server.is_empty() {
        return Err("Enter the Audiopub Icecast server.".to_string());
    }
    if port == 0 {
        return Err("Enter a valid Audiopub Icecast port.".to_string());
    }
    Ok(IcecastTarget {
        host: format!("{server}:{port}"),
        mount: identity.user_id.clone(),
        username: "source".to_string(),
        password: identity.stream_key.clone(),
        content_type: content_type.to_string(),
    })
}

fn direct_icecast_target(
    profile: &Connection,
    content_type: &str,
) -> Result<IcecastTarget, String> {
    let Connection::Icecast {
        server,
        port,
        mount,
        username,
        password,
        ..
    } = profile
    else {
        return Err("not an Icecast service".to_string());
    };
    let mount = normalize_mount(mount);
    // The server field is parsed rather than concatenated: a `host:port` typed
    // into it must not have the port field appended a second time. See
    // [`icecast::split_host_port`].
    let (host, typed_port) = icecast::split_host_port(server)?;
    let port = typed_port.unwrap_or(*port);
    if port == 0 {
        return Err("Enter a valid Icecast port.".to_string());
    }
    if mount.is_empty() {
        return Err("Enter the Icecast mount point.".to_string());
    }
    if password.is_empty() {
        return Err("Enter the Icecast password.".to_string());
    }
    let username = if username.trim().is_empty() {
        "source".to_string()
    } else {
        username.trim().to_string()
    };
    Ok(IcecastTarget {
        host: format!("{host}:{port}"),
        mount,
        username,
        password: password.clone(),
        content_type: content_type.to_string(),
    })
}

/// Checks that an Icecast host resolves, at Connect time.
///
/// A direct Icecast service has nothing to log into, so Connect was pure field
/// validation and the first thing that ever touched the network was Start
/// streaming — which is where a mistyped server surfaced, as a "Streaming
/// problem" modal saying `unknown host`, long after the dialog holding the
/// field that was wrong had been closed.
///
/// A name lookup is the most that can be checked here without harm: a real
/// handshake would claim the mount, and stock Icecast then holds it for
/// `<source-timeout>`, so the user's actual Start streaming would answer
/// `403 Mountpoint in use`.
async fn resolve_icecast_host(host: &str) -> Result<(), String> {
    let lookup = tokio::time::timeout(icecast::CONNECT_TIMEOUT, tokio::net::lookup_host(host));
    let found = match lookup.await {
        Ok(Ok(mut addresses)) => addresses.next().is_some(),
        Ok(Err(_)) => false,
        Err(_) => return Err(format!("looking up the server {host:?} timed out.")),
    };
    if !found {
        return Err(format!(
            "the server {host:?} could not be found. Check the Icecast server and port."
        ));
    }
    Ok(())
}

/// Replaces a guessed publishing endpoint with the one the instance itself
/// names, when it names a different one.
///
/// Called only when nothing has been typed into the server and port fields, so
/// the value being replaced is `live.<site host>:8000` and nothing else — see
/// `ServiceProfile::Audiopub::endpoint_is_default`. Every way of learning
/// nothing ends the same way, with the guess kept: an instance that answers what
/// was already assumed, one whose page cannot be parsed, and one that cannot be
/// reached at all are three shades of "carry on", and none of them is worth a
/// failed Connect. The log line is at debug for the same reason — a user whose
/// endpoint was right all along has nothing to read here.
async fn discover_endpoint(
    client: &AudioPubClient,
    site_url: &str,
    server: String,
    port: u16,
) -> (String, u16) {
    match client.published_endpoint().await {
        Ok(Some((host, found))) if (host.as_str(), found) != (server.as_str(), port) => {
            log::info!(
                "{site_url} publishes to {host}:{found}, not the {server}:{port} this service \
                 assumed; using {host}:{found}. Type an Icecast server and port on Setup \
                 streaming services to override this."
            );
            (host, found)
        }
        Ok(_) => (server, port),
        Err(e) => {
            log::debug!(
                "could not read {site_url}'s streaming instructions ({e}); assuming {server}:{port}"
            );
            (server, port)
        }
    }
}

async fn end_active_stream(active: &ActiveStream, connection: Option<&Connection>) {
    active.abort();
    if let Some(client) = connection.and_then(audiopub_client)
        && let Err(e) = client.end_stream(&active.stream_id).await
    {
        log::warn!("Ending stream reported: {e}");
    }
}

async fn net_loop(mut commands: tokio_mpsc::UnboundedReceiver<NetCommand>, events: EventSender) {
    let mut connection: Option<Connection> = None;
    let mut stream: Option<ActiveStream> = None;

    while let Some(command) = commands.recv().await {
        match command {
            NetCommand::Connect { profile } => {
                // Before `connection` is replaced, not after: `end_active_stream`
                // ends the server-side row through the client that started it,
                // and every later cleanup path (`Disconnect`, `StopStream`)
                // reads whatever `connection` holds *then*. Connecting over a
                // live stream used to hand the old stream's id to the new
                // client — or to no client at all, when the new service was a
                // direct Icecast target with no API — leaving the old row live
                // until the server expired it. `StartStream` has had this guard
                // for the same reason.
                if let Some(previous) = stream.take() {
                    log::warn!(
                        "Connecting to a service while a stream is live; ending the old one"
                    );
                    end_active_stream(&previous, connection.as_ref()).await;
                    let _ = events.send(NetEvent::StreamEnded);
                }
                match profile {
                    ServiceProfile::Audiopub {
                        id,
                        nickname,
                        site_url,
                        server,
                        port,
                        email,
                        password,
                        endpoint_is_default,
                    } => {
                        let client = match AudioPubClient::new(&site_url) {
                            Ok(c) => c,
                            Err(e) => {
                                let _ = events.send(NetEvent::ConnectFailed {
                                    message: e.to_string(),
                                });
                                continue;
                            }
                        };
                        // Only while nothing has been typed in, and only ever to
                        // replace the guess. See `endpoint_is_default`.
                        let (server, port) = if endpoint_is_default {
                            discover_endpoint(&client, &site_url, server, port).await
                        } else {
                            (server, port)
                        };
                        // The publishing host is configurable here as well now,
                        // so it is checked here as well: logging in proves the
                        // *site* is reachable and says nothing about the Icecast
                        // host beside it, which is not dialled until Start
                        // streaming. Same lookup, same reason, as the direct
                        // Icecast arm below.
                        let host = format!("{server}:{port}");
                        if let Err(message) = resolve_icecast_host(&host).await {
                            let _ = events.send(NetEvent::ConnectFailed { message });
                            continue;
                        }
                        match client.login(&email, password.as_str()).await {
                            Ok(()) => match client.stream_identity().await {
                                Ok(identity) => {
                                    let display_name = nickname;
                                    log::info!(
                                        "Audiopub service {display_name:?} publishes to {host}"
                                    );
                                    connection = Some(Connection::Audiopub {
                                        client: Arc::new(client),
                                        identity,
                                        server,
                                        port,
                                    });
                                    let _ = events.send(NetEvent::Connected {
                                        service_id: id,
                                        display_name,
                                    });
                                }
                                Err(e) => {
                                    let _ = events.send(NetEvent::ConnectFailed {
                                        message: format!("logged in, but no stream access: {e}"),
                                    });
                                }
                            },
                            Err(e) => {
                                let _ = events.send(NetEvent::ConnectFailed {
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                    ServiceProfile::Icecast {
                        id,
                        nickname,
                        server,
                        port,
                        mount,
                        username,
                        password,
                        listener_url,
                    } => {
                        let armed = Connection::Icecast {
                            server,
                            port,
                            mount,
                            username,
                            password,
                            listener_url,
                        };
                        let checked = match direct_icecast_target(&armed, "audio/mpeg") {
                            Ok(target) => resolve_icecast_host(&target.host)
                                .await
                                .map(|()| target.host),
                            Err(message) => Err(message),
                        };
                        match checked {
                            Ok(host) => {
                                log::info!("Icecast service {nickname:?} resolves to {host}");
                                connection = Some(armed);
                                let _ = events.send(NetEvent::Connected {
                                    service_id: id,
                                    display_name: nickname,
                                });
                            }
                            Err(message) => {
                                log::warn!("Icecast service {nickname:?}: {message}");
                                let _ = events.send(NetEvent::ConnectFailed { message });
                            }
                        }
                    }
                    ServiceProfile::Youtube {
                        id,
                        nickname,
                        ffmpeg,
                        url,
                        key,
                        image,
                        video_bitrate_kbps,
                        h264,
                        aac,
                        chat,
                    } => {
                        let target = RtmpTarget {
                            ffmpeg,
                            url,
                            key,
                            image,
                            // The real value comes from Preferences at Start
                            // streaming, where the user may have changed it
                            // since; this is only a placeholder so the target is
                            // complete. See `start_stream`.
                            audio_bitrate_kbps: 128,
                            video_bitrate_kbps,
                            h264,
                            aac,
                        };
                        // Nothing is dialled here. RTMP has no cheap "is anyone
                        // there" exchange — the handshake claims the stream key
                        // and starts a broadcast, which is precisely what
                        // Connect must not do. The checks that *can* be made
                        // without one (the binary exists, it has the encoders,
                        // the fields are filled in) all happened in
                        // `ui::service_profile_from_site`, with the dialog still
                        // open in front of the user.
                        log::info!(
                            "YouTube service {nickname:?} publishes to {} using {} and {}",
                            target.describe(),
                            target.h264,
                            target.aac
                        );
                        if let Some(chat) = &chat {
                            log::info!("YouTube service {nickname:?} reads chat from its {}", chat.describe());
                        }
                        connection = Some(Connection::Youtube { target, chat });
                        let _ = events.send(NetEvent::Connected {
                            service_id: id,
                            display_name: nickname,
                        });
                    }
                }
            }
            NetCommand::Disconnect => {
                if let Some(active) = stream.take() {
                    log::warn!("Disconnecting from a service while a stream is live; ending it");
                    end_active_stream(&active, connection.as_ref()).await;
                    // Said out loud for the same reason `Connect` says it: the
                    // UI stayed on "Streaming" otherwise, and the engine kept
                    // encoding into a channel with nothing left reading it.
                    let _ = events.send(NetEvent::StreamEnded);
                }
                connection = None;
                let _ = events.send(NetEvent::Disconnected);
            }
            NetCommand::StartStream {
                title,
                description,
                archive,
                content_type,
                audio,
                audio_bitrate_kbps,
            } => {
                if connection.is_none() {
                    let _ = events.send(NetEvent::StreamError {
                        message: t!("not connected to a streaming service"),
                    });
                    continue;
                }
                // Starting over the top of a live stream used to overwrite the
                // `ActiveStream` and leak it: its sender and chat tasks kept
                // running and the server-side row was never ended, leaving two
                // sources racing one mount — where the second is guaranteed
                // `403 Mountpoint in use`. Only reachable now that a streaming
                // failure no longer forces the UI back to Idle behind our back.
                if let Some(previous) = stream.take() {
                    log::warn!("Starting a stream while one is live; ending the old one first");
                    end_active_stream(&previous, connection.as_ref()).await;
                }
                let Some(conn) = &connection else {
                    continue;
                };
                match start_stream(
                    conn,
                    StreamRequest {
                        title: &title,
                        description: &description,
                        archive,
                        content_type: &content_type,
                        audio_bitrate_kbps,
                    },
                    audio,
                    events.clone(),
                )
                .await
                {
                    // `StreamStarted` is sent from inside `start_stream`, not
                    // here: it resets `server_stream` to `Pending`, so it has to
                    // reach the pump *before* the live-events feed can report a
                    // state. See the note at that send.
                    Ok(active) => stream = Some(active),
                    Err(message) => {
                        // The only record of a stream that never started: the
                        // modal this becomes is gone as soon as it is dismissed,
                        // and the log is what users are asked to send.
                        log::error!("Starting the stream failed: {message}");
                        let _ = events.send(NetEvent::StreamError { message });
                    }
                }
            }
            NetCommand::StopStream => {
                if let Some(active) = stream.take() {
                    end_active_stream(&active, connection.as_ref()).await;
                }
                let _ = events.send(NetEvent::StreamEnded);
            }
            NetCommand::SendChat(content) => {
                let (Some(conn), Some(active)) = (&connection, &stream) else {
                    let _ = events.send(NetEvent::ChatSendFailed {
                        message: "not streaming".into(),
                    });
                    continue;
                };
                let Some(client) = audiopub_client(conn) else {
                    let _ = events.send(NetEvent::ChatSendFailed {
                        message: chat_unavailable(conn).into(),
                    });
                    continue;
                };
                match client.send_chat(&active.stream_id, &content).await {
                    Ok(_) => {
                        let _ = events.send(NetEvent::ChatSent);
                    }
                    Err(e) => {
                        let _ = events.send(NetEvent::ChatSendFailed {
                            message: e.to_string(),
                        });
                    }
                }
            }
            NetCommand::ReconnectChat => {
                let (Some(conn), Some(active)) = (&connection, &stream) else {
                    let _ = events.send(NetEvent::ChatFeed(ChatFeedState::Interrupted {
                        reason: "not streaming".into(),
                    }));
                    continue;
                };
                // Reading is what the button is for, so a YouTube service with a
                // channel configured gets it even though it cannot send: its
                // feed is a poll loop that can wedge on a stale continuation in
                // exactly the way the Audiopub feed wedges on a half-open
                // socket, which is the case this button exists for.
                let has_feed = match conn {
                    Connection::Audiopub { .. } => true,
                    Connection::Youtube { chat, .. } => chat.is_some(),
                    Connection::Icecast { .. } => false,
                };
                if !has_feed {
                    let _ = events.send(NetEvent::ChatFeed(ChatFeedState::Interrupted {
                        reason: match conn {
                            Connection::Youtube { .. } => {
                                "this service has no YouTube channel set, so there is no chat \
                                 to reconnect"
                                    .into()
                            }
                            _ => "chat is only available for Audiopub and YouTube services".into(),
                        },
                    }));
                    continue;
                }
                log::info!("Chat feed: reconnect requested by the user");
                let _ = active.chat_reconnect.send(());
            }
            NetCommand::Shutdown => {
                if let Some(active) = stream.take() {
                    end_active_stream(&active, connection.as_ref()).await;
                }
                return;
            }
        }
    }
}

/// How long the chat feed may be silent before we treat it as dead.
///
/// The server sends `: keepalive\n\n` every 30 seconds
/// (`src/routes/live/[id]/events/+server.ts` in audiopub-sv), so three missed
/// keepalives is a connection that is not coming back. A timeout is the only
/// way to catch the *half-open* case: when a NAT gateway or router silently
/// drops an idle flow there is no FIN and no RST, so the read below never
/// resolves and never errors. Without this, that connection wedges forever and
/// chat simply stops with no error anywhere.
const CHAT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Backoff between reconnect attempts, in seconds; the last value repeats.
const CHAT_BACKOFF: [u64; 6] = [1, 2, 4, 8, 15, 30];

/// How long to wait before reconnect attempt `attempt` (0-based). Saturates at
/// the last step rather than growing without bound: a stream can be live for
/// hours, and a user who fixes their network should not then wait an hour for
/// chat to notice.
fn chat_backoff(attempt: usize) -> Duration {
    Duration::from_secs(CHAT_BACKOFF[attempt.min(CHAT_BACKOFF.len() - 1)])
}

/// Backoff between Icecast reconnect attempts, in seconds; the last repeats.
///
/// Starts at 2 rather than the chat feed's 1 because the *expected* first answer
/// after a source drops is `403 Mountpoint in use`: stock Icecast holds the
/// mount for `<source-timeout>` (default 10 s) after the source dies, so the
/// mount we want is still nominally ours. The ladder puts attempts at t+2, t+7,
/// t+17, t+32, t+52, t+82…, clearing a 10 s hold by the third attempt without
/// hammering a server that is telling us to wait.
const AUDIO_BACKOFF: [u64; 6] = [2, 5, 10, 15, 20, 30];

fn audio_backoff(attempt: usize) -> Duration {
    Duration::from_secs(AUDIO_BACKOFF[attempt.min(AUDIO_BACKOFF.len() - 1)])
}

/// How long we keep trying before giving up and ending the broadcast.
///
/// Budgeted against the server, not against patience. When a source drops,
/// audiopub-sv's `mount_remove` hook sets the stream row to `disconnected` and
/// stamps `disconnectedAt`; `StreamingService`'s poller runs every 5 minutes and
/// finishes any `disconnected` stream older than that cutoff. So the grace
/// window is 5 to 10 minutes, and `sourceConnected()` explicitly accepts a prior
/// state of `disconnected` — meaning a reconnect inside it resumes the *same*
/// stream id, chat history, listener counts and archive file.
///
/// Four minutes stops us before the earliest moment the server could expire the
/// row, so we never reconnect into a stream that has already been finished, and
/// the give-up message can say honestly that the broadcast is over.
const AUDIO_RECONNECT_BUDGET: Duration = Duration::from_secs(240);

/// When to say "still trying" — once, halfway through the budget.
const AUDIO_HALFWAY_NOTICE: Duration = Duration::from_secs(120);

/// What the supervisor should do about a failed attempt.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Wait this long, then reconnect.
    Retry(Duration),
    /// Stop. The broadcast is over.
    GiveUp { reason: String },
}

/// The retry policy, as a pure function so the ladder, the budget and the
/// terminal/retryable split are all testable without a socket or a runtime.
fn plan_retry(error: &IcecastError, attempt: usize, since_first_failure: Duration) -> Step {
    plan_retry_for(error.retryable(), || error.explain(), attempt, since_first_failure)
}

/// [`plan_retry`] without the Icecast error type.
///
/// One policy for both outgoing transports rather than two that drift: the
/// ladder, the four-minute budget and the "terminal failures give up at once"
/// rule are properties of *this app's* idea of an outage, not of Icecast. The
/// reason is a closure because building it can allocate and the common path —
/// a retryable error with budget left — never needs it.
fn plan_retry_for(
    retryable: bool,
    reason: impl Fn() -> String,
    attempt: usize,
    since_first_failure: Duration,
) -> Step {
    // A wrong stream key or a banned account will answer the same way in four
    // minutes' time, so those give up at once rather than burning the budget.
    if !retryable {
        return Step::GiveUp { reason: reason() };
    }
    let Some(left) = AUDIO_RECONNECT_BUDGET.checked_sub(since_first_failure) else {
        return Step::GiveUp { reason: reason() };
    };
    if left.is_zero() {
        return Step::GiveUp { reason: reason() };
    }
    // Never sleep past the budget: the last wait should land us on the deadline,
    // not well beyond it, so the give-up is announced when promised.
    Step::Retry(audio_backoff(attempt).min(left))
}

/// Poll interval once the server has said the stream is gone. It is not coming
/// back on its own, so this only exists to notice a server that changes its
/// mind, and to keep the task alive for the reconnect button.
const CHAT_GONE_POLL: Duration = Duration::from_secs(60);

/// Why [`read_chat_feed`] returned.
enum FeedExit {
    /// `finish` arrived: the stream is over. Stop for good.
    Finished,
    /// The connection ended, errored, or went silent past the watchdog.
    Dropped { reason: String },
    /// The user pressed the reconnect button.
    Forced,
    /// The stream is being torn down. Stop for good.
    Closed,
}

/// What ended a wait.
#[derive(PartialEq, Eq)]
enum Wake {
    /// The delay elapsed.
    Timer,
    /// The user pressed the reconnect button.
    User,
    /// Every sender is gone: the stream is being torn down.
    Closed,
}

/// Sleeps, unless the user asks for a reconnect first. Distinguishes the two,
/// so a press gets an answer rather than being folded into an outage that was
/// already reported — and so a closed channel ends the task instead of
/// spinning, since `recv` on one returns immediately and forever.
async fn wait_or_reconnect(
    reconnect: &mut tokio_mpsc::UnboundedReceiver<()>,
    delay: Duration,
) -> Wake {
    tokio::select! {
        request = reconnect.recv() => match request {
            Some(()) => {
                drain(reconnect);
                Wake::User
            }
            None => Wake::Closed,
        },
        _ = tokio::time::sleep(delay) => Wake::Timer,
    }
}

/// Discards reconnect requests that piled up, so several impatient presses
/// cost one reconnect rather than one each.
fn drain(reconnect: &mut tokio_mpsc::UnboundedReceiver<()>) {
    while reconnect.try_recv().is_ok() {}
}

/// Owns the live-events connection for one stream: opens it, reads it, and
/// reopens it for as long as the stream lives.
///
/// The retry loop lives in the task rather than in [`net_loop`] because that
/// loop only ever wakes on a command, so it cannot observe a task dying without
/// growing a `select!` around its receiver.
fn spawn_chat_feed(
    client: Arc<AudioPubClient>,
    stream_id: String,
    events: EventSender,
    mut reconnect: tokio_mpsc::UnboundedReceiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // True while an outage has already been reported, so the log gets one
        // line per outage rather than one per attempt: a stream can be gone for
        // an hour, and saying so once a minute would bury everything else.
        let mut reported = false;
        // Set when the user pressed the button. They are owed an answer even if
        // the answer is the same one already on screen, so this overrides
        // `reported` exactly once.
        let mut owed_an_answer = false;
        let mut attempt: usize = 0;

        loop {
            match client.open_events(&stream_id).await {
                Ok(EventsStream::Gone) => {
                    if !reported || owed_an_answer {
                        reported = true;
                        // No need to clear `owed_an_answer` here: the wait
                        // below overwrites it before anything can read it.
                        log::warn!(
                            "Chat feed: server reports stream {stream_id} no longer exists (204)"
                        );
                        let _ = events.send(NetEvent::ChatFeed(ChatFeedState::StreamGone));
                    }
                    // No backoff ladder: the stream is not coming back, so this
                    // settles to a slow poll rather than hammering.
                    match wait_or_reconnect(&mut reconnect, CHAT_GONE_POLL).await {
                        Wake::Closed => return,
                        wake => owed_an_answer = wake == Wake::User,
                    }
                    continue;
                }
                Ok(EventsStream::Open(response)) => {
                    if reported || owed_an_answer {
                        log::info!("Chat feed: reconnected to stream {stream_id}");
                        let _ = events.send(NetEvent::ChatFeed(ChatFeedState::Restored));
                    } else {
                        log::info!("Chat feed: connected to stream {stream_id}");
                    }
                    reported = false;
                    owed_an_answer = false;
                    attempt = 0;
                    match read_chat_feed(response, &events, &mut reconnect).await {
                        FeedExit::Finished => {
                            log::info!("Chat feed: stream {stream_id} finished");
                            return;
                        }
                        FeedExit::Forced => {
                            log::info!("Chat feed: reconnecting at the user's request");
                            owed_an_answer = true;
                            continue;
                        }
                        FeedExit::Closed => return,
                        FeedExit::Dropped { reason } => {
                            log::warn!("Chat feed: connection lost: {reason}");
                            reported = true;
                            let _ = events
                                .send(NetEvent::ChatFeed(ChatFeedState::Interrupted { reason }));
                        }
                    }
                }
                Err(e) => {
                    let reason = e.to_string();
                    log::warn!("Chat feed: could not open the events stream: {reason}");
                    if !reported || owed_an_answer {
                        reported = true;
                        owed_an_answer = false;
                        let _ =
                            events.send(NetEvent::ChatFeed(ChatFeedState::Interrupted { reason }));
                    }
                }
            }

            let wait = chat_backoff(attempt);
            attempt = attempt.saturating_add(1);
            log::info!(
                "Chat feed: retrying in {}s (attempt {attempt})",
                wait.as_secs()
            );
            match wait_or_reconnect(&mut reconnect, wait).await {
                Wake::Closed => return,
                Wake::User => owed_an_answer = true,
                Wake::Timer => {}
            }
        }
    })
}

/// Reads one live-events connection until it ends.
///
/// The reconnect doorbell is checked here as well as in the backoff, because a
/// wedged half-open connection is exactly the case the button exists for and
/// the read below would otherwise never return.
async fn read_chat_feed(
    response: reqwest::Response,
    events: &EventSender,
    reconnect: &mut tokio_mpsc::UnboundedReceiver<()>,
) -> FeedExit {
    use futures_util::StreamExt;

    let mut parser = SseParser::new();
    let mut body = response.bytes_stream();

    loop {
        // The watchdog resets on any *chunk*, not on any parsed event:
        // `SseParser::feed` drops `: keepalive` comment lines internally, so
        // keepalives yield no `SseEvent` and are invisible above the parser.
        // Bytes arriving is the only true liveness signal.
        //
        // `biased` so a waiting reconnect request wins over a chunk that is
        // already available: the button's whole purpose is to abandon this
        // connection, and on a wedged half-open socket the read never returns
        // at all.
        let next = tokio::select! {
            biased;
            request = reconnect.recv() => match request {
                Some(()) => {
                    drain(reconnect);
                    return FeedExit::Forced;
                }
                None => return FeedExit::Closed,
            },
            next = tokio::time::timeout(CHAT_IDLE_TIMEOUT, body.next()) => next,
        };
        let chunk = match next {
            Err(_) => {
                return FeedExit::Dropped {
                    reason: format!("no data for {} seconds", CHAT_IDLE_TIMEOUT.as_secs()),
                };
            }
            Ok(None) => {
                return FeedExit::Dropped {
                    reason: "the server closed the connection".to_string(),
                };
            }
            // Previously discarded with `let Ok(chunk) = chunk else { break }`,
            // which is why no failure here ever reached the log.
            Ok(Some(Err(e))) => {
                return FeedExit::Dropped {
                    reason: e.to_string(),
                };
            }
            Ok(Some(Ok(chunk))) => chunk,
        };

        for raw in parser.feed(&chunk) {
            match LiveEvent::from_sse(&raw) {
                Some(LiveEvent::Chat(message)) => {
                    let _ = events.send(NetEvent::Chat(message));
                }
                Some(LiveEvent::Listeners { active, peak }) => {
                    // We are one of the listeners (this SSE connection);
                    // do not count ourselves.
                    let _ = events.send(NetEvent::Listeners {
                        active: active.saturating_sub(1),
                        peak: peak.saturating_sub(1),
                    });
                }
                // Every value, `active` included. This used to filter `active`
                // out to keep the log quiet across reconnects, which threw away
                // the one event that says the server has accepted the source and
                // listeners can hear us -- so the UI reported a healthy stream
                // from the moment the Icecast socket opened, which is minutes too
                // early when the server's ffprobe validation is slow or failing.
                // The pump holds the last value and only acts on a change, so the
                // repeat on every connect costs nothing.
                Some(LiveEvent::State { state }) => {
                    let _ = events.send(NetEvent::ServerStreamState { state });
                }
                Some(LiveEvent::Archived) => {
                    log::info!("Chat feed: the server archived this stream");
                    let _ = events.send(NetEvent::ChatFeed(ChatFeedState::Archived));
                }
                Some(LiveEvent::Finish) => {
                    let _ = events.send(NetEvent::StreamEnded);
                    return FeedExit::Finished;
                }
                // Deleting a row would break the append-only invariant the chat
                // list relies on (`chat::append_new_messages` maps list indices
                // onto `run.chat`); it needs a full refresh, which is its own
                // change.
                Some(LiveEvent::ChatDeleted { .. }) | None => {}
            }
        }
    }
}

/// Why [`pump_audio`] returned.
enum SendExit {
    /// The encoder channel closed: the stream is being stopped normally.
    Ended,
    /// A write failed or timed out.
    Dropped { error: IcecastError },
}

/// Sends encoded audio down one connection until it ends.
async fn pump_audio(
    conn: &mut IcecastConnection,
    audio: &mut tokio_mpsc::Receiver<Vec<u8>>,
) -> SendExit {
    while let Some(chunk) = audio.recv().await {
        if let Err(error) = conn.send(&chunk).await {
            return SendExit::Dropped { error };
        }
    }
    SendExit::Ended
}

/// Owns the Icecast source connection for one stream: sends down it, and
/// reopens it for as long as the stream lives or the retry budget allows.
///
/// Mirrors [`spawn_chat_feed`], including the `reported` flag that gives the
/// log one line per outage rather than one per attempt. The retry loop
/// lives in the task for the same reason it does there: [`net_loop`] only wakes
/// on a command, so it cannot observe a task dying.
///
/// The first connection is handed in already open rather than being made here,
/// so that a wrong stream key still fails `Start streaming` loudly and at once
/// instead of becoming four minutes of quiet retrying behind a UI that claims
/// to be live.
///
/// **`audio` must not be dropped except on the terminal path.** Dropping the
/// receiver closes the channel, which the engine reads as `TrySendError::Closed`
/// and answers by stopping encoding for good — after which a successful
/// reconnect would have nothing left to send. Holding it open through an outage
/// is exactly what makes the engine's drop-newest policy do its job.
fn spawn_icecast_sender(
    target: IcecastTarget,
    first: IcecastConnection,
    mut audio: tokio_mpsc::Receiver<Vec<u8>>,
    events: EventSender,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut conn = first;
        let mut said_halfway = false;
        let mut first_failure: Option<Instant> = None;
        let mut attempt: usize = 0;

        // The same rule as the reconnect drain below, for the same reason, and
        // it belongs here too because the *first* connection is preceded by a
        // wait as well: `start_streaming` sends `StartEncoding` before
        // `StartStream`, so the engine has been filling this channel throughout
        // `create_stream` and the Icecast handshake. Sending that backlog would
        // open the broadcast with audio that is already seconds old, and it
        // lands in Icecast's burst buffer -- which is precisely what the first
        // listener is handed as their starting point, so the whole stream would
        // begin that far behind live and stay there.
        while audio.try_recv().is_ok() {}

        loop {
            let mut error = match pump_audio(&mut conn, &mut audio).await {
                SendExit::Ended => {
                    conn.close().await;
                    return;
                }
                SendExit::Dropped { error } => error,
            };
            log::warn!("Icecast source: {error}");

            let started = *first_failure.get_or_insert_with(Instant::now);
            // Exactly once per outage, because reaching here is what an outage
            // *is*: the only way back to pumping is a successful reconnect at
            // the bottom of the inner loop. A four-minute outage is two or three
            // lines in the log, not eighty.
            let _ = events.send(NetEvent::AudioLink(AudioLinkState::Interrupted {
                reason: error.explain(),
            }));

            // Reconnect until we are back, or until the budget says the
            // broadcast is over. Every failure — the one that dropped us and
            // every failed attempt after it — is judged in this one place.
            loop {
                match plan_retry(&error, attempt, started.elapsed()) {
                    Step::GiveUp { reason } => {
                        log::error!("Icecast source: giving up ({reason})");
                        let _ = events.send(NetEvent::StreamError {
                            message: t!(
                                "The audio connection could not be restored: {reason}. \
                                 The broadcast has ended.",
                                reason = reason
                            ),
                        });
                        return;
                    }
                    Step::Retry(wait) => {
                        log::info!(
                            "Icecast source: reconnecting in {}s (attempt {attempt})",
                            wait.as_secs()
                        );
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                    }
                }

                if !said_halfway && started.elapsed() >= AUDIO_HALFWAY_NOTICE {
                    said_halfway = true;
                    let _ = events.send(NetEvent::AudioLink(AudioLinkState::StillRetrying {
                        remaining_seconds: AUDIO_RECONNECT_BUDGET
                            .saturating_sub(started.elapsed())
                            .as_secs(),
                    }));
                }

                // Discard whatever the engine queued while we were down, and do
                // it *before* connecting so the discard covers the connect
                // latency too. This is a live stream: delivering the backlog
                // would push every listener permanently further behind live —
                // Icecast paces listeners by the rate the source delivers, so
                // the debt is never repaid — and would replay a stale fragment
                // of speech out of context. Resuming at live is the only
                // sensible answer; the gap is the honest cost of the outage.
                while audio.try_recv().is_ok() {}

                match IcecastConnection::connect(&target).await {
                    Ok(fresh) => {
                        conn = fresh;
                        let gap_seconds = started.elapsed().as_secs();
                        log::info!("Icecast source: reconnected after {gap_seconds}s");
                        let _ = events.send(NetEvent::AudioLink(AudioLinkState::Restored {
                            gap_seconds,
                        }));
                        said_halfway = false;
                        first_failure = None;
                        attempt = 0;
                        break;
                    }
                    Err(e) => {
                        log::warn!("Icecast source: reconnect failed ({e})");
                        error = e;
                    }
                }
            }
        }
    })
}

/// How quickly an ffmpeg exit counts as "it never got going".
///
/// A refused RTMP handshake — a wrong stream key, a stream key for a broadcast
/// that has been ended, an ingest URL with a typo in it — kills ffmpeg within a
/// second or two, every time, with no status code to read: RTMP carries no such
/// thing, and the only account of what went wrong is prose on stderr. A dropped
/// *live* session, by contrast, is preceded by minutes of successful publishing.
const RTMP_EARLY_EXIT: Duration = Duration::from_secs(6);

/// How many consecutive early exits mean the settings are wrong rather than the
/// network being bad.
///
/// Three, not one: a machine coming out of sleep, or a Wi-Fi link that has not
/// finished associating, will refuse the first connection just as fast as a bad
/// key does. Three in a row across the backoff ladder is seventeen seconds of
/// the same instant refusal, which no transient outage looks like — and stopping
/// there means a mistyped stream key is a message in twenty seconds instead of
/// four minutes of a UI claiming to be reconnecting.
const RTMP_EARLY_EXIT_LIMIT: u32 = 3;

/// Sends PCM into one ffmpeg until it stops taking it.
///
/// Also watches for the child having exited, which the write alone does not
/// catch: a pipe with room in its buffer accepts a write to a dead process, so a
/// broadcast could go on being "sent" into a kernel buffer for as long as that
/// buffer lasted. Not on every block — `try_wait` is a system call and the
/// mixer produces a hundred blocks a second — but often enough that the outage
/// is noticed in a fraction of a second.
async fn pump_rtmp(process: &mut RtmpProcess, audio: &mut tokio_mpsc::Receiver<Vec<u8>>) -> RtmpExit {
    const CHECK_EVERY: u32 = 25;
    let mut since_check = 0u32;
    while let Some(chunk) = audio.recv().await {
        if let Err(error) = process.send(&chunk).await {
            return RtmpExit::Dropped { error };
        }
        since_check += 1;
        if since_check >= CHECK_EVERY {
            since_check = 0;
            if let Some(error) = process.exited() {
                return RtmpExit::Dropped { error };
            }
        }
    }
    RtmpExit::Ended
}

/// Why [`pump_rtmp`] returned.
enum RtmpExit {
    /// The engine's channel closed: the stream is being stopped normally.
    Ended,
    Dropped { error: RtmpError },
}

/// Owns the ffmpeg publisher for one stream, restarting it for as long as the
/// stream lives or the retry budget allows.
///
/// The sibling of [`spawn_icecast_sender`], and every rule there applies here
/// for the same reason: **`audio` must not be dropped except on the terminal
/// path** (dropping it closes the channel, which the engine reads as "stop
/// producing" and answers permanently, so a successful restart would have
/// nothing to send), the queued backlog is discarded before every restart
/// (delivering it would put the broadcast permanently behind live), and the
/// first process is handed in already started so a missing ffmpeg fails `Start
/// streaming` loudly.
///
/// The one rule that is *not* shared is the early-exit rule above. Icecast
/// answers a bad key with `401` and `IcecastError::retryable` can say so at
/// once; RTMP answers with a closed socket that is indistinguishable from a
/// network failure, so the same judgement has to be made from timing instead.
fn spawn_rtmp_sender(
    target: RtmpTarget,
    first: RtmpProcess,
    mut audio: tokio_mpsc::Receiver<Vec<u8>>,
    events: EventSender,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut process = first;
        let mut said_halfway = false;
        let mut first_failure: Option<Instant> = None;
        let mut attempt: usize = 0;
        let mut started_at = Instant::now();
        let mut early_exits = 0u32;

        // The same discard, for the same reason, as the Icecast sender's: the
        // engine has been filling this channel since `StartRawFeed`, throughout
        // ffmpeg's launch and its RTMP handshake. Sending it would open the
        // broadcast several seconds behind live and leave it there.
        while audio.try_recv().is_ok() {}

        loop {
            let mut error = match pump_rtmp(&mut process, &mut audio).await {
                RtmpExit::Ended => {
                    process.close().await;
                    return;
                }
                RtmpExit::Dropped { error } => error,
            };
            // Whatever is left of it: its output is already gone, so there is
            // nothing worth waiting to flush.
            process.abandon().await;

            if started_at.elapsed() < RTMP_EARLY_EXIT {
                early_exits += 1;
            } else {
                early_exits = 0;
            }
            log::warn!("RTMP: {}", error.explain());

            let started = *first_failure.get_or_insert_with(Instant::now);
            let _ = events.send(NetEvent::AudioLink(AudioLinkState::Interrupted {
                reason: error.explain(),
            }));

            loop {
                if early_exits >= RTMP_EARLY_EXIT_LIMIT {
                    let reason = error.explain();
                    log::error!("RTMP: giving up after {early_exits} immediate failures ({reason})");
                    let _ = events.send(NetEvent::StreamError {
                        message: t!(
                            "FFmpeg could not publish to {target}. Check the stream key and the \
                             ingest URL for this service. The broadcast has ended.\n\n{reason}",
                            target = target.describe(),
                            reason = reason
                        ),
                    });
                    return;
                }
                match plan_retry_for(error.retryable(), || error.explain(), attempt, started.elapsed())
                {
                    Step::GiveUp { reason } => {
                        log::error!("RTMP: giving up ({reason})");
                        let _ = events.send(NetEvent::StreamError {
                            message: t!(
                                "The connection to {target} could not be restored: {reason}. \
                                 The broadcast has ended.",
                                target = target.describe(),
                                reason = reason
                            ),
                        });
                        return;
                    }
                    Step::Retry(wait) => {
                        log::info!("RTMP: restarting FFmpeg in {}s (attempt {attempt})", wait.as_secs());
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                    }
                }

                if !said_halfway && started.elapsed() >= AUDIO_HALFWAY_NOTICE {
                    said_halfway = true;
                    let _ = events.send(NetEvent::AudioLink(AudioLinkState::StillRetrying {
                        remaining_seconds: AUDIO_RECONNECT_BUDGET
                            .saturating_sub(started.elapsed())
                            .as_secs(),
                    }));
                }

                // Before the restart, so the discard covers the launch and the
                // handshake as well as the outage itself.
                while audio.try_recv().is_ok() {}

                match RtmpProcess::start(&target).await {
                    Ok(fresh) => {
                        process = fresh;
                        started_at = Instant::now();
                        let gap_seconds = started.elapsed().as_secs();
                        log::info!("RTMP: FFmpeg restarted after {gap_seconds}s");
                        let _ = events.send(NetEvent::AudioLink(AudioLinkState::Restored {
                            gap_seconds,
                        }));
                        said_halfway = false;
                        first_failure = None;
                        attempt = 0;
                        break;
                    }
                    Err(e) => {
                        log::warn!("RTMP: restart failed ({})", e.explain());
                        // A launch failure is instant by definition, so it
                        // counts towards the early-exit limit as well: three
                        // attempts to run a binary that is not there is enough.
                        early_exits += 1;
                        error = e;
                    }
                }
            }
        }
    })
}

/// Owns the YouTube chat poll for one broadcast.
///
/// Mirrors [`spawn_chat_feed`] — the `reported` flag that keeps the log to one
/// line per outage, the reconnect doorbell, the backoff ladder — with two
/// differences that come from what it is reading.
///
/// The first is that **the start-up window is not an outage.** A YouTube
/// broadcast does not exist as a watch page the moment ffmpeg's RTMP session is
/// accepted; YouTube takes the better part of a minute to promote it, and until
/// then the channel's `/live` page answers "nothing is live" and the chat is
/// switched off. Reporting each of those as an interruption would fill the log
/// with failures during the most normal thing that happens. So nothing is
/// reported as an outage until the feed has worked at least once.
///
/// The second is that this feed is also the only thing that can say whether
/// YouTube is *serving* the broadcast — the RTMP session says only that the
/// ingest took the bytes, exactly as an open Icecast socket says only that
/// Icecast did. Resolving the video to a live watch page is the equivalent of
/// Audio Pub's `active`, and is what this reports.
fn spawn_youtube_chat(
    reference: ChannelRef,
    events: EventSender,
    mut reconnect: tokio_mpsc::UnboundedReceiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match youtube::client() {
            Ok(client) => client,
            Err(e) => {
                log::warn!("YouTube chat: no HTTP client ({e}); this stream will have no chat");
                return;
            }
        };
        let described = reference.describe();
        let mut reported = false;
        let mut owed_an_answer = false;
        // Until this is set, a "not live yet" is the expected answer rather than
        // a failure. See the doc comment.
        let mut ever_opened = false;
        let mut attempt: usize = 0;

        loop {
            match youtube::Chat::open(client.clone(), &reference).await {
                Ok(mut chat) => {
                    if reported || owed_an_answer {
                        log::info!("YouTube chat: reconnected to {}", chat.video_id);
                        let _ = events.send(NetEvent::ChatFeed(ChatFeedState::Restored));
                    } else {
                        log::info!(
                            "YouTube chat: reading {} ({})",
                            youtube::watch_url(&chat.video_id),
                            described
                        );
                    }
                    reported = false;
                    owed_an_answer = false;
                    ever_opened = true;
                    attempt = 0;
                    // The broadcast has a live watch page, which is the only
                    // evidence available that YouTube is serving it.
                    let _ = events.send(NetEvent::ServerStreamState {
                        state: "active".to_string(),
                    });

                    match read_youtube_chat(&mut chat, &events, &mut reconnect).await {
                        FeedExit::Forced => {
                            log::info!("YouTube chat: reconnecting at the user's request");
                            owed_an_answer = true;
                            continue;
                        }
                        FeedExit::Closed | FeedExit::Finished => return,
                        FeedExit::Dropped { reason } => {
                            log::warn!("YouTube chat: {reason}");
                            reported = true;
                            let _ = events
                                .send(NetEvent::ChatFeed(ChatFeedState::Interrupted { reason }));
                        }
                    }
                }
                Err(e) if !e.retryable() => {
                    // Only a reference that cannot be parsed reaches here, and
                    // it will not parse in a minute either.
                    log::warn!("YouTube chat: {e}; this stream will have no chat");
                    let _ = events.send(NetEvent::ChatFeed(ChatFeedState::Interrupted {
                        reason: e.to_string(),
                    }));
                    return;
                }
                Err(e) => {
                    if !ever_opened {
                        // The start-up window. Logged, because a stream that
                        // never gets chat needs an explanation somewhere, but
                        // not announced as an interruption of something that has
                        // not started.
                        log::info!("YouTube chat: waiting for {described} ({e})");
                    } else {
                        let reason = e.to_string();
                        log::warn!("YouTube chat: {reason}");
                        if !reported || owed_an_answer {
                            reported = true;
                            owed_an_answer = false;
                            let _ = events
                                .send(NetEvent::ChatFeed(ChatFeedState::Interrupted { reason }));
                        }
                    }
                }
            }

            let wait = chat_backoff(attempt);
            attempt = attempt.saturating_add(1);
            match wait_or_reconnect(&mut reconnect, wait).await {
                Wake::Closed => return,
                Wake::User => owed_an_answer = true,
                Wake::Timer => {}
            }
        }
    })
}

/// Polls one open chat until it stops answering.
///
/// The wait between polls is the server's own `timeoutMs`, not a rate of our
/// choosing — YouTube's chat panel is told how often to come back and so are we.
/// The reconnect doorbell is checked in the same `select!` for the same reason
/// the Audiopub feed checks it: a continuation that has gone stale answers
/// forever without ever erroring, and the button exists for exactly that.
async fn read_youtube_chat(
    chat: &mut youtube::Chat,
    events: &EventSender,
    reconnect: &mut tokio_mpsc::UnboundedReceiver<()>,
) -> FeedExit {
    loop {
        let batch = match chat.poll().await {
            Ok(batch) => batch,
            Err(e) => {
                return FeedExit::Dropped {
                    reason: e.to_string(),
                };
            }
        };
        for item in batch.messages {
            let _ = events.send(NetEvent::Chat(item.into_message()));
        }
        if let Some(active) = batch.viewers {
            // No peak of YouTube's own, so the count is its own high-water mark
            // and the pump takes the maximum from there.
            let _ = events.send(NetEvent::Listeners {
                active,
                peak: active,
            });
        }
        // `biased` so a waiting reconnect request wins over the timer.
        tokio::select! {
            biased;
            request = reconnect.recv() => match request {
                Some(()) => {
                    drain(reconnect);
                    return FeedExit::Forced;
                }
                None => return FeedExit::Closed,
            },
            _ = tokio::time::sleep(batch.wait) => {}
        }
    }
}

/// How often a direct Icecast mount's listener count is refreshed.
///
/// This is the one polled thing in `net`, and it is polled because Icecast
/// offers nothing else: a source client's connection carries audio one way and
/// the server never volunteers anything about who is listening. Audio Pub's
/// counts arrive over SSE precisely because Audio Pub built a channel for them;
/// a plain mount has only the status document.
///
/// Ten seconds is chosen against the **sound events** rather than the display.
/// A listener arriving or leaving plays a cue (`StreamEvent::ListenerIncrease`
/// and its siblings), and a cue that lands half a minute after the fact reads as
/// a cue for nothing — while the Home tab's number is read on demand and would
/// be happy with much less. The document is a couple of kilobytes.
const LISTENER_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Bounds one status request, well inside the interval so a wedged read cannot
/// stack up behind the next one. A reading is only worth having while it is
/// current, so a slow answer is better dropped than waited for.
const LISTENER_POLL_TIMEOUT: Duration = Duration::from_secs(8);

/// How many consecutive readings may omit the mount before the log says so.
///
/// Not one: at the moment a stream starts, the mount genuinely is not up yet —
/// and when the count is being taken on a *relay*, that relay has to notice the
/// source and connect before it appears at all, which is seconds at best. A
/// minute of silence separates "still coming up" from "that mount name is
/// wrong", which is the misconfiguration this whole field invites.
const LISTENER_MISSING_GRACE: u32 = 6;

/// Owns the listener-count poll for one direct Icecast stream.
///
/// Failures are the log's business and nothing else's: this reports a number
/// beside a broadcast that is running perfectly well, so an unreachable status
/// document must not become a modal, a chat line, or a stream state. The same
/// `reported` flag the chat feed and the Icecast sender carry keeps that to one
/// line per outage rather than one every ten seconds for the whole show.
fn spawn_listener_poll(
    target: stats::StatsTarget,
    events: EventSender,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(LISTENER_POLL_TIMEOUT)
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                log::warn!("Listener counts: no HTTP client ({e}); the count will stay at zero");
                return;
            }
        };
        let counting = match &target.mount {
            Some(mount) => mount.clone(),
            None => "every mount on the server".to_string(),
        };
        log::info!(
            "Listener counts: reading {} every {}s, counting {counting}",
            target.status_url,
            LISTENER_POLL_INTERVAL.as_secs()
        );

        let mut reported = false;
        // Stock Icecast's *public* status document carries no peak, so the
        // high-water mark is ours to keep. The UI takes a max of its own, but
        // doing it here as well means the number this task reports is true on
        // its own terms rather than only after the pump has seen it.
        let mut high_water = 0u32;
        let mut missing = 0u32;
        let mut said_missing = false;

        loop {
            match read_counts(&client, &target).await {
                Ok(counts) => {
                    if reported {
                        reported = false;
                        log::info!("Listener counts: {} is answering again", target.status_url);
                    }
                    if counts.matched {
                        missing = 0;
                        if said_missing {
                            said_missing = false;
                            log::info!("Listener counts: {counting} is up; counting it again");
                        }
                    } else {
                        missing = missing.saturating_add(1);
                        if missing >= LISTENER_MISSING_GRACE && !said_missing {
                            said_missing = true;
                            log::warn!(
                                "Listener counts: {} does not list {counting}. Check the \
                                 listener count URL for this service; the count stays at zero \
                                 until that mount appears.",
                                target.status_url
                            );
                        }
                    }
                    high_water = high_water.max(counts.listeners);
                    let _ = events.send(NetEvent::Listeners {
                        active: counts.listeners,
                        peak: counts.peak.max(high_water),
                    });
                }
                Err(reason) => {
                    if !reported {
                        reported = true;
                        log::warn!(
                            "Listener counts: could not read {} ({reason})",
                            target.status_url
                        );
                    }
                }
            }
            tokio::time::sleep(LISTENER_POLL_INTERVAL).await;
        }
    })
}

/// One reading of a status document.
async fn read_counts(
    client: &reqwest::Client,
    target: &stats::StatsTarget,
) -> Result<stats::Counts, String> {
    let response = client
        .get(&target.status_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        // Read before the body is touched: a 404 here is the likeliest single
        // failure — a server old enough or locked down enough to have no JSON
        // status document — and its body is an HTML error page that would only
        // muddy the message.
        return Err(format!("the server answered {status}"));
    }
    let body = response.text().await.map_err(|e| e.to_string())?;
    stats::parse_counts(&body, target.mount.as_deref())
}

/// What one [`NetCommand::StartStream`] asks for, minus the channels.
///
/// Grouped because these travel together and mean nothing apart: a stream's
/// title, its description and whether to archive it are one decision the user
/// made in the Set stream info dialog, and the two encoder fields describe the
/// same bytes from either end. Passing them as one borrowed struct also keeps
/// [`start_stream`] readable now that a third service type reads a different
/// subset of them than the other two.
struct StreamRequest<'a> {
    title: &'a str,
    description: &'a str,
    archive: bool,
    content_type: &'a str,
    audio_bitrate_kbps: u32,
}

async fn start_stream(
    conn: &Connection,
    request: StreamRequest<'_>,
    audio: tokio_mpsc::Receiver<Vec<u8>>,
    events: EventSender,
) -> Result<ActiveStream, String> {
    let StreamRequest {
        title,
        description,
        archive,
        content_type,
        audio_bitrate_kbps,
    } = request;
    match conn {
        Connection::Audiopub {
            client,
            identity,
            server,
            port,
            ..
        } => {
            // Timed because the start sequence is the one place a user cannot
            // see what is taking the time, and the two halves fail differently:
            // a slow `create_stream` is the site, a slow handshake is Icecast.
            // Together with the time-to-accepted the UI logs once the server
            // says `active`, this is what separates "the server's ffprobe was
            // merely slow" from "the server killed the source and we
            // reconnected" when reading a log after the fact.
            let began = Instant::now();
            let stream_id = client
                .create_stream(title, description, archive)
                .await
                .map_err(|e| e.to_string())?;
            log::info!(
                "Stream start: created stream {stream_id} in {} ms",
                began.elapsed().as_millis()
            );

            let target = audiopub_target_for(server, *port, identity, content_type)?;
            // The first connect stays here, and inline, so a wrong stream key or
            // a banned account fails `Start streaming` at once with a reason.
            // Every *later* connect happens inside the sender task.
            let handshake = Instant::now();
            let icecast = IcecastConnection::connect(&target)
                .await
                .map_err(|e| e.to_string())?;
            log::info!(
                "Stream start: Icecast accepted the source in {} ms ({} ms since Start)",
                handshake.elapsed().as_millis(),
                began.elapsed().as_millis()
            );
            let icecast_task = spawn_icecast_sender(target, icecast, audio, events.clone());

            // Announced *before* the feed exists, and this ordering is the whole
            // point of it being here rather than in the caller. The pump's
            // `StreamStarted` arm resets `server_stream` to `Pending`, while the
            // feed's first act is to report the state the server already holds —
            // so a feed that got its answer first would have it overwritten by
            // the announcement of the stream it belongs to. The server re-sends
            // `state` only on connect and on transition, which makes that a
            // one-way trap: the stream would read "waiting for the server to
            // accept" for the whole of its life. Nothing below can fail, so this
            // is not sent for a stream that then does not start.
            let _ = events.send(NetEvent::StreamStarted {
                stream_id: stream_id.clone(),
            });

            // The feed opens itself. Chat failing is not a reason to refuse to
            // broadcast, so unlike the Icecast connection above, a live-events
            // endpoint that will not open is reported in the log and retried
            // rather than failing the whole stream start.
            let (chat_reconnect, reconnect_rx) = tokio_mpsc::unbounded_channel();
            let sse_task = spawn_chat_feed(
                client.clone(),
                stream_id.clone(),
                events.clone(),
                reconnect_rx,
            );

            Ok(ActiveStream {
                stream_id,
                sse_task: Some(sse_task),
                // Audio Pub counts listeners itself and says so over the feed
                // above; nothing to poll.
                stats_task: None,
                icecast_task,
                chat_reconnect,
            })
        }
        Connection::Icecast {
            server,
            port,
            mount,
            listener_url,
            ..
        } => {
            let target = direct_icecast_target(conn, content_type)?;
            let icecast = IcecastConnection::connect(&target)
                .await
                .map_err(|e| e.to_string())?;
            let icecast_task = spawn_icecast_sender(target, icecast, audio, events.clone());

            let stream_id = format!("icecast:{}", normalize_mount(mount));
            // No feed to race here — a direct mount has none — but the two arms
            // announce the same way so there is one answer to "where is
            // `StreamStarted` sent".
            let _ = events.send(NetEvent::StreamStarted {
                stream_id: stream_id.clone(),
            });

            // Counts are worth having and are not worth a broadcast. The field
            // is validated in `service_profile_from_site`, so this can only fail
            // on a service edited between Connect and Start — in which case the
            // Home tab reads zero listeners and the log says why, rather than
            // Start streaming failing over a number.
            let stats_task = match stats::stats_target(server, *port, mount, listener_url) {
                Ok(target) => Some(spawn_listener_poll(target, events.clone())),
                Err(reason) => {
                    log::warn!("Listener counts: {reason} The count will stay at zero.");
                    None
                }
            };

            Ok(ActiveStream {
                stream_id,
                sse_task: None,
                stats_task,
                // Never rung: a direct Icecast mount has no chat feed at all.
                chat_reconnect: tokio_mpsc::unbounded_channel().0,
                icecast_task,
            })
        }
        Connection::Youtube { target, chat } => {
            // The bitrate is the only part of the target the user could have
            // changed between Connect and Start, because it lives in
            // Preferences rather than on the service.
            let mut target = target.clone();
            target.audio_bitrate_kbps = audio_bitrate_kbps;

            let began = Instant::now();
            // Started here rather than inside the sender for the same reason the
            // first Icecast connect is: an ffmpeg that will not run at all
            // should fail `Start streaming` at once, with a reason, rather than
            // becoming four minutes of quiet retrying behind a UI claiming to be
            // live. What it cannot catch is a refused RTMP handshake — that
            // happens after the process exists, and `spawn_rtmp_sender`'s
            // early-exit rule is what turns it into a fast answer.
            let process = RtmpProcess::start(&target)
                .await
                .map_err(|e| e.explain())?;
            log::info!(
                "Stream start: FFmpeg publishing to {} in {} ms",
                target.describe(),
                began.elapsed().as_millis()
            );
            let stream_id = chat
                .as_ref()
                .and_then(|reference| match reference {
                    ChannelRef::Video(id) => Some(youtube::stream_id(id)),
                    ChannelRef::Channel(_) => None,
                })
                .unwrap_or_else(|| "youtube".to_string());
            let icecast_task = spawn_rtmp_sender(target, process, audio, events.clone());

            // Before the chat task, exactly as the Audiopub arm announces before
            // its feed: the pump's `StreamStarted` arm resets `server_stream` to
            // `Pending`, and the chat task's first act is to report `active` the
            // moment YouTube is serving the broadcast. The other order would
            // overwrite that with `Pending` and leave it there for good.
            let _ = events.send(NetEvent::StreamStarted {
                stream_id: stream_id.clone(),
            });

            let (chat_reconnect, reconnect_rx) = tokio_mpsc::unbounded_channel();
            let sse_task = match chat {
                Some(reference) => Some(spawn_youtube_chat(
                    reference.clone(),
                    events.clone(),
                    reconnect_rx,
                )),
                None => {
                    // Nothing can say whether the ingest is being served, so the
                    // Home tab is told so rather than being left reading
                    // "waiting for the server to accept the stream" for the whole
                    // broadcast. The same answer a direct Icecast mount gets, for
                    // the same reason.
                    log::info!(
                        "No YouTube channel is set for this service, so there is no chat and \
                         no way to tell when YouTube starts serving the broadcast"
                    );
                    let _ = events.send(NetEvent::ServerStreamState {
                        state: "unknown".to_string(),
                    });
                    None
                }
            };

            Ok(ActiveStream {
                stream_id,
                sse_task,
                // YouTube's viewer count arrives on the chat feed when it
                // arrives at all, the way Audio Pub's does; there is nothing
                // separate to poll.
                stats_task: None,
                icecast_task,
                chat_reconnect,
            })
        }
    }
}

/// A real broadcast against a real Audiopub instance, start to finish.
///
/// This is the only test that proves the thing the unit tests can only imply:
/// that an endpoint discovered from an instance's own page is one the instance
/// will actually accept a source connection on. Everything below the discovery
/// is the ordinary path — create the stream, dial Icecast, feed it MP3, watch
/// the state arrive over SSE, end the stream.
///
/// It **puts a live broadcast on the site**, briefly and publicly, so it is
/// gated on `PUBSPLASH_TEST_BROADCAST=1` on top of `--ignored` and the three
/// credential variables. Nothing here holds a credential; they all come from the
/// environment. Run with:
///
/// ```text
/// cargo test --bin pubsplash real_broadcast -- --ignored --nocapture
/// ```
#[cfg(test)]
mod real_broadcast_tests {
    use super::*;
    use crate::audio::encoder::Mp3Encoder;
    use crate::audio::mixer::{CHANNELS, SAMPLE_RATE};

    /// Long enough to cover `sourceConnected`'s serial validation — archiving,
    /// then `ffprobe -probesize 33000` against the live mount, which needs about
    /// two seconds of *real-time* audio at 128 kbps, then a third fetch just to
    /// read a content-type header.
    const DEADLINE: Duration = Duration::from_secs(60);
    const BITRATE_KBPS: u32 = 128;
    /// The mixer's own block, so the pacing matches what the engine really does.
    const BLOCK_FRAMES: usize = SAMPLE_RATE as usize / 100;

    /// A quiet 440 Hz tone rather than digital silence: it proves audio is
    /// flowing rather than that a socket is open, and it is what anyone who
    /// happened to be listening would hear, so it stays at about -30 dBFS.
    fn tone_block(phase: &mut f32) -> Vec<i16> {
        let step = std::f32::consts::TAU * 440.0 / SAMPLE_RATE as f32;
        let mut pcm = Vec::with_capacity(BLOCK_FRAMES * CHANNELS);
        for _ in 0..BLOCK_FRAMES {
            let sample = (phase.sin() * 0.03 * i16::MAX as f32) as i16;
            *phase = (*phase + step) % std::f32::consts::TAU;
            for _ in 0..CHANNELS {
                pcm.push(sample);
            }
        }
        pcm
    }

    #[tokio::test]
    #[ignore]
    async fn real_broadcast_round_trip() {
        use futures_util::StreamExt;

        let (Ok(site), Ok(email), Ok(password)) = (
            std::env::var("PUBSPLASH_TEST_SITE"),
            std::env::var("PUBSPLASH_TEST_EMAIL"),
            std::env::var("PUBSPLASH_TEST_PASSWORD"),
        ) else {
            eprintln!("skipped: set PUBSPLASH_TEST_SITE/_EMAIL/_PASSWORD to run this");
            return;
        };
        if std::env::var("PUBSPLASH_TEST_BROADCAST").as_deref() != Ok("1") {
            eprintln!("skipped: this goes live on the site; set PUBSPLASH_TEST_BROADCAST=1");
            return;
        }

        let client = AudioPubClient::new(&site).unwrap();
        let (host, port) = client
            .published_endpoint()
            .await
            .expect("instructions page")
            .expect("an endpoint");
        client.login(&email, &password).await.expect("login");
        let identity = client.stream_identity().await.expect("stream key");
        eprintln!("publishing to {host}:{port}, mount {}", identity.user_id);

        let stream_id = client
            .create_stream(
                "Pubsplash connection test",
                "Automated check that this instance accepts a source connection.",
                false,
            )
            .await
            .expect("create stream");
        eprintln!("stream {stream_id} created");

        // Opened before the source connects, so the `state` transition the
        // server sends on `sourceConnected` cannot be missed.
        let events = match client.open_events(&stream_id).await.expect("open events") {
            EventsStream::Open(response) => response,
            EventsStream::Gone => panic!("the stream was gone the moment it was created"),
        };

        let target = IcecastTarget {
            host: format!("{host}:{port}"),
            mount: identity.user_id.clone(),
            username: "source".to_string(),
            password: identity.stream_key.clone(),
            content_type: "audio/mpeg".to_string(),
        };
        let mut source = IcecastConnection::connect(&target)
            .await
            .expect("icecast source connection");
        eprintln!("source connection accepted");

        let mut encoder = Mp3Encoder::new(BITRATE_KBPS).expect("encoder");
        let mut parser = SseParser::new();
        let mut body = events.bytes_stream();
        let mut phase = 0.0f32;
        let started = Instant::now();
        let mut went_active = false;
        let mut ticker = tokio::time::interval(Duration::from_millis(10));

        while started.elapsed() < DEADLINE && !went_active {
            // Paced like the engine: Icecast reads a source at the rate it
            // delivers, so sending as fast as the loop can encode would put the
            // whole broadcast ahead of real time.
            ticker.tick().await;
            let mp3 = encoder.encode(&tone_block(&mut phase)).expect("encode");
            if !mp3.is_empty() {
                source.send(mp3).await.expect("send audio");
            }
            // Non-blocking: the audio must keep flowing while we wait for the
            // server to finish validating it.
            if let Ok(Some(chunk)) = tokio::time::timeout(Duration::ZERO, body.next()).await {
                for raw in parser.feed(&chunk.expect("event chunk")) {
                    match LiveEvent::from_sse(&raw) {
                        Some(LiveEvent::State { state }) => {
                            eprintln!("  state: {state} (at {:?})", started.elapsed());
                            went_active |= state == "active";
                        }
                        Some(LiveEvent::Listeners { active, peak }) => {
                            eprintln!("  listeners: {active} (peak {peak})");
                        }
                        other => eprintln!("  event: {other:?}"),
                    }
                }
            }
        }

        source.close().await;
        client.end_stream(&stream_id).await.expect("end stream");
        eprintln!("stream {stream_id} ended");

        assert!(
            went_active,
            "the server never reported the stream active within {DEADLINE:?}; \
             the source connection was accepted on {host}:{port} but the audio \
             was not validated"
        );
    }
}

#[cfg(test)]
mod chat_feed_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The whole point of the feature: a source whose connection dies mid-send
    /// reopens it and carries on, rather than ending the broadcast.
    ///
    /// Time is paused so the backoff ladder costs nothing; the sockets are real,
    /// so the handshake, the drop and the second connect are the genuine ones.
    #[tokio::test(start_paused = true)]
    async fn sender_reconnects_after_the_server_drops_the_source() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // The second connection is reported over a channel rather than by
        // returning, so the server task can hold that socket open afterwards.
        // Letting it close would be a second, genuine outage, and the
        // one-line-per-outage assertion below would be measuring the test's own
        // teardown instead of the sender's behaviour.
        let (report_tx, report_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            // First source connection: handshake, take some audio, then vanish
            // the way a dropped link does.
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .unwrap();
            let _ = sock.read(&mut buf).await;
            drop(sock);

            // Second: the reconnect.
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = vec![0u8; 4096];
            let n = sock.read(&mut head).await.unwrap();
            let request = String::from_utf8_lossy(&head[..n]).to_string();
            sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .unwrap();
            let mut audio = vec![0u8; 4];
            sock.read_exact(&mut audio).await.unwrap();
            let _ = report_tx.send((request, audio));

            // Hold it open, and keep draining so the sender never stalls.
            loop {
                if sock.read(&mut buf).await.unwrap_or(0) == 0 {
                    std::future::pending::<()>().await;
                }
            }
        });

        let target = IcecastTarget {
            host: addr.to_string(),
            mount: "user-123".into(),
            username: "source".into(),
            password: Secret::new("key"),
            content_type: "audio/mpeg".into(),
        };
        let first = IcecastConnection::connect(&target).await.unwrap();

        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (audio_tx, audio_rx) = tokio_mpsc::channel(200);
        let task = spawn_icecast_sender(target, first, audio_rx, EventSender(event_tx));

        // Keep offering audio. The first blocks go into the doomed connection;
        // once it fails, the sender must open a new one and send what follows.
        let feeder = tokio::spawn(async move {
            for _ in 0..200 {
                if audio_tx.send(b"MP3!".to_vec()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let (request, audio) = report_rx.await.unwrap();
        assert!(request.starts_with("PUT /user-123 HTTP/1.1\r\n"));
        // The reconnect re-authenticates with the same stored key, which is why
        // no re-login is needed: the stream key is a long-lived per-user value.
        assert!(request.contains("Authorization: Basic c291cmNlOmtleQ=="));
        assert_eq!(&audio, b"MP3!");

        feeder.abort();
        task.abort();
        server.abort();

        let events: Vec<_> = event_rx.try_iter().collect();
        let interrupted = events
            .iter()
            .filter(|e| matches!(e, NetEvent::AudioLink(AudioLinkState::Interrupted { .. })))
            .count();
        let restored = events
            .iter()
            .filter(|e| matches!(e, NetEvent::AudioLink(AudioLinkState::Restored { .. })))
            .count();
        assert_eq!(interrupted, 1, "one line per outage, not one per attempt");
        assert_eq!(restored, 1, "the recovery must be announced");
        // The broadcast must not have been declared over.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, NetEvent::StreamError { .. })),
            "a recoverable blip must never end the stream"
        );
    }

    /// A terminal rejection must end the broadcast at once rather than retry
    /// for four minutes: a revoked stream key or an expired stream row will
    /// answer exactly the same way every time.
    #[tokio::test(start_paused = true)]
    async fn a_terminal_rejection_ends_the_stream_without_retrying() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = attempts.clone();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .unwrap();
            let _ = sock.read(&mut buf).await;
            drop(sock);
            // Every reconnect is refused with a reason that cannot change.
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.0 401 Unauthorized\r\n\
                          icecast-auth-message: No active stream found\r\n\r\n",
                    )
                    .await;
            }
        });

        let target = IcecastTarget {
            host: addr.to_string(),
            mount: "u".into(),
            username: "source".into(),
            password: Secret::new("key"),
            content_type: "audio/mpeg".into(),
        };
        let first = IcecastConnection::connect(&target).await.unwrap();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (audio_tx, audio_rx) = tokio_mpsc::channel(200);
        let task = spawn_icecast_sender(target, first, audio_rx, EventSender(event_tx));

        let feeder = tokio::spawn(async move {
            for _ in 0..200 {
                if audio_tx.send(b"MP3!".to_vec()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        // The task returns of its own accord once it gives up.
        tokio::time::timeout(Duration::from_secs(600), task)
            .await
            .expect("the sender must give up rather than retry for ever")
            .unwrap();
        feeder.abort();
        server.abort();

        let events: Vec<_> = event_rx.try_iter().collect();
        let message = events
            .iter()
            .find_map(|e| match e {
                NetEvent::StreamError { message } => Some(message.clone()),
                _ => None,
            })
            .expect("the broadcast must be reported as over");
        // The server's own words reach the user, so they know it is their
        // stream that expired and not their network that failed.
        assert!(message.contains("No active stream found"), "{message}");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a terminal rejection must not be retried"
        );
    }

    /// `start_streaming` starts the encoder before it asks the network for
    /// anything, so by the time this task exists the channel already holds
    /// everything encoded during `create_stream` and the Icecast handshake.
    /// Sending it would open the broadcast seconds behind live and *stay* there:
    /// it lands in Icecast's burst buffer, which is exactly what the first
    /// listener is handed as their starting point.
    #[tokio::test(start_paused = true)]
    async fn the_first_connection_drops_the_backlog_encoded_before_it_existed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (report_tx, report_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .unwrap();
            // Whatever arrives first is what a listener would hear first.
            let mut first = vec![0u8; 4];
            sock.read_exact(&mut first).await.unwrap();
            let _ = report_tx.send(first);
            loop {
                if sock.read(&mut buf).await.unwrap_or(0) == 0 {
                    std::future::pending::<()>().await;
                }
            }
        });

        let target = IcecastTarget {
            host: addr.to_string(),
            mount: "u".into(),
            username: "source".into(),
            password: Secret::new("key"),
            content_type: "audio/mpeg".into(),
        };
        let first = IcecastConnection::connect(&target).await.unwrap();

        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let (audio_tx, audio_rx) = tokio_mpsc::channel(200);
        // Stale: encoded while the handshake above was still in flight.
        for _ in 0..50 {
            audio_tx.send(b"OLD!".to_vec()).await.unwrap();
        }
        let task = spawn_icecast_sender(target, first, audio_rx, EventSender(event_tx));

        // Give the task its first poll, so the drain runs before anything fresh
        // is offered — the ordering the real engine produces, where the backlog
        // predates the connection by whole seconds.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        audio_tx.send(b"NEW!".to_vec()).await.unwrap();

        let heard = report_rx.await.unwrap();
        assert_eq!(
            &heard, b"NEW!",
            "listeners must start at live, not at whatever was queued before the socket opened"
        );

        task.abort();
        server.abort();
    }

    #[test]
    fn audio_backoff_climbs_then_saturates() {
        assert_eq!(audio_backoff(0), Duration::from_secs(2));
        assert_eq!(audio_backoff(2), Duration::from_secs(10));
        assert_eq!(audio_backoff(5), Duration::from_secs(30));
        assert_eq!(audio_backoff(500), Duration::from_secs(30));
    }

    /// A fast reconnect races our own dying socket: stock Icecast holds the
    /// mount for `<source-timeout>` (default 10 s) after a source drops, so the
    /// ladder has to reach past that without hammering a server saying "wait".
    #[test]
    fn audio_backoff_clears_the_mount_hold_by_the_third_attempt() {
        let elapsed: u64 = (0..3).map(|a| audio_backoff(a).as_secs()).sum();
        assert!(elapsed >= 10, "third attempt lands at t+{elapsed}s");
    }

    /// The budget exists to stop *before* the server can finish the stream:
    /// `StreamingService`'s poller runs every 5 minutes and ends any stream
    /// that has been `disconnected` longer than that, so retrying past the
    /// 5-minute floor would reconnect into a stream that no longer exists.
    #[test]
    fn retry_budget_stops_before_the_servers_poller() {
        assert!(AUDIO_RECONNECT_BUDGET < Duration::from_secs(5 * 60));
        assert!(AUDIO_HALFWAY_NOTICE < AUDIO_RECONNECT_BUDGET);
    }

    fn rejected(status: u16, line: &str) -> IcecastError {
        IcecastError::Rejected {
            status,
            status_line: line.into(),
            message: None,
        }
    }

    #[test]
    fn terminal_errors_give_up_without_burning_the_budget() {
        // A wrong stream key answers the same way in four minutes' time.
        let step = plan_retry(
            &rejected(401, "HTTP/1.0 401 Unauthorized"),
            0,
            Duration::ZERO,
        );
        assert!(matches!(step, Step::GiveUp { .. }));
    }

    #[test]
    fn retryable_errors_inside_the_budget_are_retried() {
        let step = plan_retry(
            &rejected(403, "HTTP/1.0 403 Mountpoint in use"),
            0,
            Duration::from_secs(3),
        );
        assert_eq!(step, Step::Retry(Duration::from_secs(2)));
    }

    #[test]
    fn retryable_errors_past_the_budget_give_up() {
        let err = IcecastError::Timeout {
            what: "sending audio",
        };
        assert!(matches!(
            plan_retry(&err, 9, AUDIO_RECONNECT_BUDGET),
            Step::GiveUp { .. }
        ));
        assert!(matches!(
            plan_retry(&err, 9, AUDIO_RECONNECT_BUDGET + Duration::from_secs(60)),
            Step::GiveUp { .. }
        ));
    }

    /// The last wait must land on the deadline, not well past it, so the
    /// give-up is announced when the message says it will be.
    #[test]
    fn a_retry_never_sleeps_past_the_budget() {
        let err = IcecastError::Timeout {
            what: "sending audio",
        };
        let left = Duration::from_secs(3);
        let step = plan_retry(&err, 5, AUDIO_RECONNECT_BUDGET - left);
        assert_eq!(step, Step::Retry(left));
    }

    #[test]
    fn backoff_climbs_then_saturates() {
        assert_eq!(chat_backoff(0), Duration::from_secs(1));
        assert_eq!(chat_backoff(3), Duration::from_secs(8));
        assert_eq!(chat_backoff(5), Duration::from_secs(30));
        // A stream can be live for hours; the wait must not grow with it.
        assert_eq!(chat_backoff(500), Duration::from_secs(30));
    }

    /// Three missed keepalives. The server's interval is 30s, hard-coded in
    /// `src/routes/live/[id]/events/+server.ts`, so this is a measured bound
    /// rather than a guess — but it must stay comfortably above 30s or a merely
    /// quiet stream would reconnect on every keepalive gap.
    #[test]
    fn idle_timeout_clears_the_servers_keepalive_interval() {
        assert!(CHAT_IDLE_TIMEOUT >= Duration::from_secs(90));
    }
}

#[cfg(test)]
mod listener_poll_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one canned status document per connection, and reports back the
    /// path each request asked for.
    fn status_server(
        body: &'static str,
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let (path_tx, path_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 2048];
                let read = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let _ = path_tx.send(path);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr.to_string(), path_rx, server)
    }

    const THREE_MOUNTS: &str = r#"{"icestats":{"source":[
        {"mount":"/live.mp3","listeners":0},
        {"mount":"/stream.mp3","listeners":5}]}}"#;

    /// The end-to-end shape of the feature: the source holds `/live.mp3`, the
    /// audience is on the relay's `/stream.mp3`, and the count the UI is handed
    /// is the relay's.
    #[tokio::test]
    async fn the_poll_reports_the_relay_mount_not_the_source_mount() {
        let (addr, mut paths, server) = status_server(THREE_MOUNTS);
        let (host, port) = icecast::split_host_port(&addr).unwrap();
        let target = stats::stats_target(&host, port.unwrap(), "live.mp3", "stream.mp3").unwrap();

        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let task = spawn_listener_poll(target, EventSender(event_tx));

        let event = tokio::task::spawn_blocking(move || {
            event_rx.recv_timeout(Duration::from_secs(10)).unwrap()
        })
        .await
        .unwrap();
        task.abort();
        server.abort();

        // The status document, not the mount, is what was fetched.
        assert_eq!(paths.recv().await.unwrap(), "/status-json.xsl");
        match event {
            NetEvent::Listeners { active, peak } => {
                assert_eq!(active, 5, "the relay mount's audience, not the source's");
                assert_eq!(peak, 5, "with no published peak, our own high-water mark");
            }
            other => panic!("expected a listener count, got {other:?}"),
        }
    }

    /// A server that answers but does not list the mount is a count of zero,
    /// not a broken poll: it must go on reporting, so the number recovers by
    /// itself the moment the relay comes up.
    #[tokio::test]
    async fn a_mount_that_is_not_up_still_reports() {
        let (addr, _paths, server) = status_server(r#"{"icestats":{}}"#);
        let (host, port) = icecast::split_host_port(&addr).unwrap();
        let target = stats::stats_target(&host, port.unwrap(), "live.mp3", "stream.mp3").unwrap();

        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let task = spawn_listener_poll(target, EventSender(event_tx));
        let event = tokio::task::spawn_blocking(move || {
            event_rx.recv_timeout(Duration::from_secs(10)).unwrap()
        })
        .await
        .unwrap();
        task.abort();
        server.abort();

        assert!(matches!(event, NetEvent::Listeners { active: 0, peak: 0 }));
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn audiopub_target_uses_configured_server_and_site_identity() {
        let identity = StreamIdentity {
            user_id: "user-123".to_string(),
            stream_key: Secret::new("stream-key"),
        };
        let target =
            audiopub_target_for("stream.example.org", 9000, &identity, "audio/mpeg").unwrap();
        assert_eq!(target.host, "stream.example.org:9000");
        assert_eq!(target.mount, "user-123");
        assert_eq!(target.username, "source");
        assert_eq!(target.password.as_str(), "stream-key");
        assert_eq!(target.content_type, "audio/mpeg");
    }

    #[test]
    fn direct_icecast_target_uses_profile_fields() {
        let conn = Connection::Icecast {
            server: "ice.example.org".to_string(),
            port: 9000,
            mount: "/live".to_string(),
            username: "dj".to_string(),
            password: Secret::new("secret"),
            listener_url: String::new(),
        };
        let target = direct_icecast_target(&conn, "audio/aac").unwrap();
        assert_eq!(target.host, "ice.example.org:9000");
        assert_eq!(target.mount, "live");
        assert_eq!(target.username, "dj");
        assert_eq!(target.password.as_str(), "secret");
        assert_eq!(target.content_type, "audio/aac");
    }

    /// The reported bug: `host:port` typed into the server field had the port
    /// field appended to it, producing `gomsen.com:8000:8000` and an
    /// unknown-host failure at Start streaming.
    #[test]
    fn a_port_in_the_server_field_is_not_appended_twice() {
        let conn = Connection::Icecast {
            server: "gomsen.com:8000".to_string(),
            port: 8000,
            mount: "live.mp3".to_string(),
            username: "source".to_string(),
            password: Secret::new("secret"),
            listener_url: String::new(),
        };
        let target = direct_icecast_target(&conn, "audio/mpeg").unwrap();
        assert_eq!(target.host, "gomsen.com:8000");
        assert_eq!(target.mount, "live.mp3");
    }

    /// The port typed alongside the host wins over the port field, since it is
    /// the more specific of the two things the user wrote.
    #[test]
    fn a_pasted_listen_url_reaches_the_right_host_and_port() {
        let conn = Connection::Icecast {
            server: "http://ice.example.org:9000/live".to_string(),
            port: 8000,
            mount: "live".to_string(),
            username: "dj".to_string(),
            password: Secret::new("secret"),
            listener_url: String::new(),
        };
        let target = direct_icecast_target(&conn, "audio/mpeg").unwrap();
        assert_eq!(target.host, "ice.example.org:9000");
    }

    #[test]
    fn direct_icecast_target_defaults_blank_username() {
        let conn = Connection::Icecast {
            server: "ice.example.org".to_string(),
            port: 8000,
            mount: "live".to_string(),
            username: String::new(),
            password: Secret::new("secret"),
            listener_url: String::new(),
        };
        let target = direct_icecast_target(&conn, "audio/mpeg").unwrap();
        assert_eq!(target.username, "source");
    }

    #[test]
    fn direct_icecast_target_accepts_the_root_mount() {
        let conn = Connection::Icecast {
            server: "radio.example.org".to_string(),
            port: 8000,
            mount: "/".to_string(),
            username: "source".to_string(),
            password: Secret::new("secret"),
            listener_url: String::new(),
        };
        let target = direct_icecast_target(&conn, "audio/mpeg").unwrap();
        assert_eq!(target.mount, "/");
    }
}
