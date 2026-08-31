//! The Mastodon worker: one long-lived thread for posting, a short-lived one
//! per authorization, and the flood gate that both go through.
//!
//! Nothing here runs on the UI thread. `reqwest` is async and the callers are
//! blocking, so everything bridges through [`block_on`] on a `current_thread`
//! runtime — the same arrangement, and for the same reason, as `tts/net.rs`
//! (the crate deliberately has no `rt-multi-thread`; see the note in
//! `Cargo.toml`).
//!
//! **The anti-flood gate lives in [`post`], not in any caller.** There is no
//! other route to `api::post_status`, so however wrong a future scheduler gets,
//! it cannot post to someone's timeline more than once per
//! [`MIN_POST_INTERVAL`](super::MIN_POST_INTERVAL). That is a deliberate choice
//! of where to put the check: a gate the callers opt into is a gate that a new
//! caller forgets.
//!
//! The gate is kept **per account**, because an account is what a timeline
//! belongs to. Every streaming service carries its own Mastodon account now, so
//! a single global stamp would have let a stop-and-restart onto another service
//! refuse a first announcement that no timeline had yet seen — a flood gate
//! silently eating the one post the user was waiting for.

use super::api::{self, Link, MastodonError};
use super::{MIN_POST_INTERVAL, TemplateKind};
use crate::secret::Secret;
use crossbeam_channel::{Receiver, Sender};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
/// When the last post to each account *succeeded*, keyed by [`PostJob::gate_key`].
/// A refused post does not move its stamp, so a run of failures cannot lock the
/// user out.
///
/// It only ever grows, by one entry per account the user has actually posted
/// from in this session — a handful at the very most.
static LAST_POST: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());
static POSTS: OnceLock<Sender<PostJob>> = OnceLock::new();

/// Runs `future` to completion, blocking the calling thread.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("building the Mastodon runtime")
    });
    runtime.block_on(future)
}

pub fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

/// Which announcement a result belongs to, so the UI can word its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occasion {
    Start,
    Continuation,
    Resume,
}

impl Occasion {
    pub fn describe(self) -> &'static str {
        match self {
            Occasion::Start => "start-of-stream",
            Occasion::Continuation => "still-streaming",
            Occasion::Resume => "resumed-stream",
        }
    }
}

impl From<TemplateKind> for Occasion {
    fn from(kind: TemplateKind) -> Self {
        match kind {
            TemplateKind::Start => Occasion::Start,
            TemplateKind::Continuation => Occasion::Continuation,
        }
    }
}

/// What the pump reads off `App::mastodon_rx`.
#[derive(Debug)]
pub struct PostResult {
    /// The streaming service whose account this was posted from, so the pump can
    /// stamp the right one. Carried through rather than read back off the
    /// connection: a post outstanding while the user switches services would
    /// otherwise land its stamp on the wrong account's settings.
    pub service_id: String,
    pub occasion: Occasion,
    pub result: Result<(), MastodonError>,
}

struct PostJob {
    service_id: String,
    instance: String,
    /// `@user@host`, only ever used to key the flood gate.
    account: String,
    token: Secret,
    status: String,
    key: String,
    occasion: Occasion,
    reply: Sender<PostResult>,
}

impl PostJob {
    /// What [`LAST_POST`] is keyed by: the account, which is the timeline being
    /// protected. An account Pubsplash has never been told the name of falls
    /// back to the instance, which is a coarser gate rather than none at all.
    fn gate_key(&self) -> String {
        if self.account.trim().is_empty() {
            self.instance.clone()
        } else {
            self.account.trim().to_string()
        }
    }
}

/// Queues a post. Returns immediately; the outcome arrives on `reply`.
///
/// Posts are serialized through one worker so they go out in the order they
/// were asked for, and so the flood gate below sees them one at a time.
pub fn post(
    service_id: String,
    instance: String,
    account: String,
    token: Secret,
    status: String,
    key: String,
    occasion: Occasion,
    reply: Sender<PostResult>,
) {
    let sender = POSTS.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<PostJob>();
        std::thread::Builder::new()
            .name("mastodon".into())
            .spawn(move || post_loop(rx))
            .expect("spawning the Mastodon worker");
        tx
    });
    if sender
        .send(PostJob {
            service_id,
            instance,
            account,
            token,
            status,
            key,
            occasion,
            reply,
        })
        .is_err()
    {
        log::warn!("The Mastodon worker is gone; dropping a post");
    }
}

fn post_loop(jobs: Receiver<PostJob>) {
    while let Ok(job) = jobs.recv() {
        let result = send_one(&job);
        match &result {
            Ok(()) => log::info!(
                "Posted the {} announcement to Mastodon",
                job.occasion.describe()
            ),
            Err(MastodonError::RateLimited) => log::warn!(
                "Refused a {} Mastodon post: less than {}s since the last one. \
                 This is the flood gate; something asked to post twice in a row.",
                job.occasion.describe(),
                MIN_POST_INTERVAL.as_secs()
            ),
            Err(error) => log::warn!(
                "Could not post the {} announcement to Mastodon: {error}",
                job.occasion.describe()
            ),
        }
        let _ = job.reply.send(PostResult {
            service_id: job.service_id.clone(),
            occasion: job.occasion,
            result,
        });
        wxdragon::wake_up_idle();
    }
}

fn send_one(job: &PostJob) -> Result<(), MastodonError> {
    if job.instance.is_empty() || job.token.is_empty() {
        return Err(MastodonError::NotLinked);
    }
    // The gate. The whole map is held across the request so two jobs can never
    // both pass it — the worker is single-threaded, so nothing else is waiting
    // on it anyway.
    let key = job.gate_key();
    let mut last = LAST_POST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = last.get(&key)
        && previous.elapsed() < MIN_POST_INTERVAL
    {
        return Err(MastodonError::RateLimited);
    }
    let result = block_on(api::post_status(
        &job.instance,
        &job.token,
        &job.status,
        &job.key,
    ));
    if result.is_ok() {
        last.insert(key, Instant::now());
    }
    result
}

/// Forgets the last-post time. Tests only — the gate is process-global, so
/// without this one test's post would refuse the next test's.
#[cfg(test)]
pub fn reset_gate() {
    LAST_POST.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// What an authorization attempt reports back, in order.
#[derive(Debug)]
pub enum AuthEvent {
    /// The app is registered and the browser should be sent here. The UI thread
    /// opens it — `ShellExecuteW` wants an apartment, and the UI thread already
    /// has one.
    Open(String),
    /// The loopback listener could not be used, so the user has to copy the code
    /// out of their browser. Send it (or `None` to cancel) back on the channel.
    NeedCode(Sender<Option<String>>),
    Done(Result<Link, MastodonError>),
}

/// Starts an authorization. Events arrive on `reply` until `Done`.
///
/// Its own thread, not the post worker's queue: the user may sit on the consent
/// screen for minutes, and a stream announcement must not wait behind that.
pub fn authorize(instance: String, reply: Sender<AuthEvent>, cancel: super::oauth::Cancel) {
    let worker_reply = reply.clone();
    std::thread::Builder::new()
        .name("mastodon-auth".into())
        .spawn(move || {
            let result = super::oauth::run(&instance, &worker_reply, &cancel);
            let _ = worker_reply.send(AuthEvent::Done(result));
            // The waiting dialog polls its receiver on a timer, but the idle
            // doorbell keeps the rest of the app responsive either way.
            wxdragon::wake_up_idle();
        })
        .map(|_| ())
        .unwrap_or_else(|e| {
            let _ = reply.send(AuthEvent::Done(Err(MastodonError::Network(format!(
                "could not start the authorization thread: {e}"
            )))));
        });
}

/// Revokes a token and forgets it, on its own thread. Best effort — the config
/// is cleared by the caller either way, so there is nothing to report.
pub fn revoke_in_background(instance: String, client_id: String, secret: Secret, token: Secret) {
    if instance.is_empty() || token.is_empty() {
        return;
    }
    std::thread::Builder::new()
        .name("mastodon-revoke".into())
        .spawn(move || {
            match block_on(api::revoke(&instance, &client_id, &secret, &token)) {
                Ok(()) => log::info!("Revoked the Mastodon access token"),
                // Worth a line, not worth a dialog: the local copy is already
                // gone, so the user is unlinked whatever the server says.
                Err(error) => log::warn!("Could not revoke the Mastodon access token: {error}"),
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "@me@example.test";

    fn job(instance: &str, token: &str) -> PostJob {
        job_for(instance, token, ACCOUNT)
    }

    fn job_for(instance: &str, token: &str, account: &str) -> PostJob {
        PostJob {
            service_id: "service-1".into(),
            instance: instance.into(),
            account: account.into(),
            token: Secret::new(token),
            status: "hi".into(),
            key: "k".into(),
            occasion: Occasion::Continuation,
            reply: crossbeam_channel::unbounded().0,
        }
    }

    fn stamp(key: &str, when: Instant) {
        LAST_POST
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), when);
    }

    fn stamped(key: &str) -> Option<Instant> {
        LAST_POST
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
    }

    /// The gate is what stops a bug upstream becoming a flood on someone's
    /// timeline, so it gets a test that never reaches a server.
    ///
    /// One test rather than several: [`LAST_POST`] is process-global by design,
    /// so separate `#[test]`s would run in parallel and reset it under each
    /// other.
    #[test]
    fn posts_are_refused_before_they_can_reach_the_network() {
        reset_gate();

        // Not linked: refused, and the gate is left untouched so the first real
        // post after linking is not made to wait.
        assert_eq!(send_one(&job("", "token")), Err(MastodonError::NotLinked));
        assert_eq!(
            send_one(&job("https://example.test", "")),
            Err(MastodonError::NotLinked)
        );
        assert!(stamped(ACCOUNT).is_none());

        // Inside the interval: refused. The host would fail loudly (and slowly)
        // if the gate ever let one through, which is the point.
        stamp(ACCOUNT, Instant::now());
        let linked = job("https://invalid.invalid", "token");
        assert_eq!(send_one(&linked), Err(MastodonError::RateLimited));
        // A refusal must not extend the window, or a busy app would starve.
        let before = stamped(ACCOUNT).unwrap();
        assert_eq!(send_one(&linked), Err(MastodonError::RateLimited));
        assert_eq!(stamped(ACCOUNT).unwrap(), before);

        // Another account's timeline has seen nothing, so it is not made to
        // wait. This is the case that broke when every streaming service shared
        // one stamp: stopping a stream and starting another somewhere else ate
        // the first announcement.
        let elsewhere = job_for("https://invalid.invalid", "token", "@me@other.test");
        assert_ne!(send_one(&elsewhere), Err(MastodonError::RateLimited));

        // An account with no name falls back to the instance rather than to no
        // gate at all.
        let unnamed = job_for("https://invalid.invalid", "token", "   ");
        stamp("https://invalid.invalid", Instant::now());
        assert_eq!(send_one(&unnamed), Err(MastodonError::RateLimited));

        // Outside the interval the gate opens again.
        stamp(ACCOUNT, Instant::now() - MIN_POST_INTERVAL);
        assert_ne!(send_one(&linked), Err(MastodonError::RateLimited));

        reset_gate();
    }

    #[test]
    fn occasions_describe_themselves() {
        assert_eq!(Occasion::from(TemplateKind::Start), Occasion::Start);
        assert_eq!(
            Occasion::from(TemplateKind::Continuation),
            Occasion::Continuation
        );
        for occasion in [Occasion::Start, Occasion::Continuation, Occasion::Resume] {
            assert!(!occasion.describe().is_empty());
        }
    }
}
