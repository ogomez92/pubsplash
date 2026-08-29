//! One media scheduler: a thread that plays files at wall-clock times into a
//! source's ring, and the handle the UI drives it with.
//!
//! The same shape as [`super::player`] — a worker per source in the active
//! scene, feeding [`ExternalFeeds`] by source name, waking on a `recv_timeout`
//! so a stop is never waited out — and it shares that module's decoder
//! ([`super::decode::stream_file`]). What differs is everything about *when*:
//! a media player is never idle while its scene is active, and a scheduler is
//! idle almost always.
//!
//! ## Firing
//!
//! Being due is decided by [`super::schedule`], which is pure and tested. The
//! rule this file adds is that each item remembers the **Unix second of the
//! occurrence it last fired**, so:
//!
//! - a tick missed while another item was playing still fires, because the
//!   occurrence has not moved;
//! - the same occurrence never fires twice, however often the check runs;
//! - and an occurrence the machine slept through is recorded as fired *without
//!   being played* once it is more than [`schedule::STALE_GRACE`] old, because a
//!   time announcement that is twenty minutes late is not late, it is wrong.
//!
//! Startup **primes** every item with the occurrence that is already in the
//! past. Without it a scheduler would fire the last hour's chime every time its
//! scene became active, and switching scenes back and forth would ring the hour
//! repeatedly.
//!
//! ## Overlap
//!
//! Two items due at once (a quarter-hourly jingle meeting the hour chime) play
//! in order rather than talking over each other, so the queue is where a due
//! item lands rather than the turntable. [`MAX_QUEUED_ITEMS`] caps it: a file
//! longer than its own interval would otherwise grow the queue forever, and a
//! schedule that has fallen that far behind is misconfigured rather than busy.
//!
//! ## Sleeping
//!
//! There is no event behind a wall clock, so this does have to look — but it
//! looks *at the clock it is waiting for* rather than on a fixed tick: an idle
//! worker sleeps until the next occurrence is due, capped by [`MAX_SLEEP`] so a
//! clock correction or a DST shift is noticed within the minute rather than
//! after however many hours the old answer had left to run.

use crate::audio::{ExternalFeeds, FeedResult};
use crate::config::ScheduleItem;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::schedule::{self, LocalTime, Trigger};
use super::{decode, track_title};

/// How far ahead of the mixer this worker decodes, in samples. The Media
/// Player's reasoning applies unchanged: everything already in the ring will be
/// heard, so a deep queue is latency on every control that stops it.
const MAX_QUEUED: usize =
    (crate::audio::mixer::SAMPLE_RATE as usize / 4) * crate::audio::mixer::CHANNELS;

/// How long to wait for a command when the only other thing to wait for is the
/// ring draining.
const RING_WAIT: Duration = Duration::from_millis(50);

/// How long to wait between attempts when the source's ring is not there yet.
const NO_RING_WAIT: Duration = Duration::from_millis(100);

/// How long a missing ring is tolerated before it is worth a log line.
const NO_RING_PATIENCE: Duration = Duration::from_secs(3);

/// The longest an idle worker sleeps before looking at the clock again, even
/// when the next item is hours off. See the module header.
const MAX_SLEEP: Duration = Duration::from_secs(30);

/// How often the schedule is consulted while a file is playing. The check is
/// integer arithmetic and one `GetLocalTime`, but the feed loop runs it
/// hundreds of times a second, and no trigger is finer than a minute.
const DUE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// How many due items may be waiting to play. Beyond this the oldest is
/// dropped, with a log line: the schedule is asking for more audio than there
/// is time to play it in.
const MAX_QUEUED_ITEMS: usize = 8;

/// What a scheduler is doing, for the source's label in the Sources list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SchedulerState {
    /// No item can fire: the list is empty, or every item is disabled or has no
    /// file. A configured source with nothing to do, not an error.
    #[default]
    Nothing,
    /// Waiting for the next item.
    Waiting,
    Playing,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub state: SchedulerState,
    /// The file playing, without its extension.
    pub playing: Option<String>,
    /// The next item due, without its extension, and the local time it is due
    /// at as `HH:MM`.
    ///
    /// Published by the worker rather than worked out by the caller because
    /// answering it means walking every item's trigger, and the caller is the
    /// UI thread building a list row.
    pub next: Option<(String, String)>,
}

/// What the UI can ask a running scheduler to do.
pub enum Command {
    /// The source's settings changed. The new items are primed against the
    /// clock, exactly as they are at startup, so editing a schedule at 09:30
    /// cannot fire the 09:00 item.
    Reload { items: Vec<ScheduleItem> },
}

/// A running scheduler. Dropping the handle does **not** stop the thread — use
/// [`Scheduler::stop`], which is what guarantees the old worker has let go of
/// the ring before a new one can be pushing into it. Same rule, and the same
/// reason, as [`super::player::Player::stop`].
pub struct Scheduler {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// What this worker was started with, so the UI can tell whether a config
    /// edit actually changed anything.
    pub items: Vec<ScheduleItem>,
}

impl Scheduler {
    pub fn start(
        source_name: String,
        feeds: ExternalFeeds,
        items: Vec<ScheduleItem>,
        generation: Arc<AtomicU64>,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let status = Arc::new(Mutex::new(Status::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("media-scheduler".into())
            .spawn({
                let mut worker = Worker {
                    source: source_name,
                    feeds,
                    commands: rx,
                    status: status.clone(),
                    generation,
                    stop: stop.clone(),
                    items: Vec::new(),
                    queue: VecDeque::new(),
                    last_due_check: None,
                };
                let items = items.clone();
                move || {
                    worker.load(items);
                    worker.run();
                }
            })
            .ok();
        Self {
            commands: tx,
            status,
            stop,
            thread,
            items,
        }
    }

    pub fn status(&self) -> Status {
        crate::audio::device::lock_recovering(&self.status, "Media scheduler status").clone()
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// The worker's thread id, so a test can tell "this scheduler is still the
    /// one that was running" from "it was stopped and started again".
    #[cfg(test)]
    pub fn thread_id(&self) -> Option<std::thread::ThreadId> {
        self.thread.as_ref().map(|t| t.thread().id())
    }

    /// Stops the worker and waits for it to let go of the ring.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.commands);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How a step ended: what the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Carry on with what you were doing.
    Go,
    /// The schedule has been replaced; abandon what is playing.
    ///
    /// A reload stops the current file rather than letting it finish because
    /// the file may be one the user has just removed from the list, and the way
    /// to stop a scheduled item playing has to be to take it out of the
    /// schedule.
    Reloaded,
    /// The worker is finished.
    Stop,
}

/// One item, plus the occurrence of it this worker has already dealt with.
struct Armed {
    item: ScheduleItem,
    /// Unix seconds of the last occurrence fired *or* deliberately skipped.
    /// `None` only before the first prime, which happens before `run`.
    fired: Option<u64>,
}

struct Worker {
    source: String,
    feeds: ExternalFeeds,
    commands: Receiver<Command>,
    status: Arc<Mutex<Status>>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    items: Vec<Armed>,
    /// Files due and waiting for the one before them to finish.
    queue: VecDeque<PathBuf>,
    /// When the schedule was last consulted, so the check inside the feed loop
    /// runs at [`DUE_CHECK_INTERVAL`] rather than per packet.
    last_due_check: Option<Instant>,
}

impl Worker {
    /// Takes a new schedule and primes every item against the clock, so nothing
    /// already in the past fires. See the module header.
    fn load(&mut self, items: Vec<ScheduleItem>) {
        self.load_at(items, LocalTime::now());
    }

    fn load_at(&mut self, items: Vec<ScheduleItem>, now: LocalTime) {
        self.items = items
            .into_iter()
            .map(|item| {
                let fired = item.trigger.last_occurrence(now).to_unix();
                Armed { item, fired }
            })
            .collect();
        self.last_due_check = None;
        self.queue.clear();
        log::info!(
            "Media scheduler {:?}: {} item(s), {} of them live",
            self.source,
            self.items.len(),
            self.items.iter().filter(|a| a.is_live()).count()
        );
    }

    fn run(&mut self) {
        loop {
            if self.stopping() {
                return;
            }
            self.collect_due();
            match self.queue.pop_front() {
                Some(file) => {
                    self.set_status(SchedulerState::Playing, Some(track_title(&file)));
                    if self.play(&file) == Flow::Stop {
                        return;
                    }
                }
                None => {
                    self.set_status(
                        if self.items.iter().any(Armed::is_live) {
                            SchedulerState::Waiting
                        } else {
                            SchedulerState::Nothing
                        },
                        None,
                    );
                    if self.wait(self.idle_sleep()) == Flow::Stop {
                        return;
                    }
                }
            }
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// How long to sleep with nothing playing: until the next item is due, or
    /// [`MAX_SLEEP`], whichever is sooner.
    fn idle_sleep(&self) -> Duration {
        let now = LocalTime::now();
        let Some(now_unix) = now.to_unix() else {
            return MAX_SLEEP;
        };
        self.next_due(now)
            .and_then(|(_, at)| at.to_unix())
            .map(|at| Duration::from_secs(at.saturating_sub(now_unix)))
            .unwrap_or(MAX_SLEEP)
            .min(MAX_SLEEP)
    }

    /// The next item due and when, for the sleep above and the source's label.
    fn next_due(&self, now: LocalTime) -> Option<(&ScheduleItem, LocalTime)> {
        self.items
            .iter()
            .filter(|armed| armed.is_live())
            .filter_map(|armed| {
                let at = armed.item.trigger.next_occurrence(now);
                at.to_unix().map(|unix| (unix, &armed.item, at))
            })
            .min_by_key(|(unix, _, _)| *unix)
            .map(|(_, item, at)| (item, at))
    }

    /// Moves every item whose occurrence has come round onto the queue.
    ///
    /// Rate-limited to [`DUE_CHECK_INTERVAL`], because this is called from the
    /// feed loop as well as the idle loop.
    fn collect_due(&mut self) {
        if self
            .last_due_check
            .is_some_and(|at| at.elapsed() < DUE_CHECK_INTERVAL)
        {
            return;
        }
        self.last_due_check = Some(Instant::now());

        let now = LocalTime::now();
        let Some(now_unix) = now.to_unix() else {
            return;
        };
        self.collect_due_at(now, now_unix);
    }

    /// The decision itself, with the clock passed in rather than read.
    ///
    /// Separated for the same reason [`crate::schedule::Schedule::stage`] is:
    /// every awkward case here is a question about a particular moment — an
    /// occurrence that has just come round, one the machine slept through, one
    /// already fired — and none of them can be tested against a clock that is
    /// whatever the test happened to run at.
    fn collect_due_at(&mut self, now: LocalTime, now_unix: u64) {
        let mut due: Vec<PathBuf> = Vec::new();
        for armed in &mut self.items {
            if !armed.is_live() {
                continue;
            }
            // A `None` here is the hour a spring-forward skipped: that instant
            // genuinely did not happen, so there is nothing to fire.
            let Some(occurrence) = armed.item.trigger.last_occurrence(now).to_unix() else {
                continue;
            };
            if armed.fired == Some(occurrence) {
                continue;
            }
            armed.fired = Some(occurrence);
            let late_by = now_unix.saturating_sub(occurrence);
            if late_by > schedule::STALE_GRACE {
                log::warn!(
                    "Media scheduler {:?}: skipping {} — it was due {late_by}s ago",
                    self.source,
                    armed.item.file
                );
                continue;
            }
            due.push(PathBuf::from(armed.item.file.trim()));
        }
        for file in due {
            if self.queue.len() >= MAX_QUEUED_ITEMS {
                // The schedule is asking for more audio than the day has room
                // for. Dropping the oldest keeps the most recently due item,
                // which is the one whose time is still roughly now.
                if let Some(dropped) = self.queue.pop_front() {
                    log::warn!(
                        "Media scheduler {:?}: dropping {} — {MAX_QUEUED_ITEMS} items are already waiting",
                        self.source,
                        dropped.display()
                    );
                }
            }
            log::info!(
                "Media scheduler {:?}: {} is due",
                self.source,
                file.display()
            );
            self.queue.push_back(file);
        }
    }

    /// Publishes what this scheduler is doing.
    fn set_status(&mut self, state: SchedulerState, playing: Option<String>) {
        let now = LocalTime::now();
        let next = self.next_due(now).map(|(item, at)| {
            (
                track_title(Path::new(item.file.trim())),
                schedule::format_time(at.minute_of_day()),
            )
        });
        let next_status = Status {
            state,
            playing,
            next,
        };
        let mut status =
            crate::audio::device::lock_recovering(&self.status, "Media scheduler status");
        if *status == next_status {
            return;
        }
        *status = next_status;
        drop(status);
        // The pump notices the counter moved and re-derives the source's label;
        // see `ui::scheduler`.
        self.generation.fetch_add(1, Ordering::Relaxed);
        wxdragon::wake_up_idle();
    }

    /// Waits up to `timeout` for a command, acting on whatever arrives.
    fn wait(&mut self, timeout: Duration) -> Flow {
        match self.commands.recv_timeout(timeout) {
            Ok(command) => self.apply(command),
            Err(RecvTimeoutError::Timeout) => {
                if self.stopping() {
                    Flow::Stop
                } else {
                    Flow::Go
                }
            }
            // The handle is gone, which only happens through `Scheduler::stop`.
            Err(RecvTimeoutError::Disconnected) => Flow::Stop,
        }
    }

    /// Acts on everything already queued, without waiting.
    fn drain_commands(&mut self) -> Flow {
        loop {
            match self.commands.try_recv() {
                Ok(command) => match self.apply(command) {
                    Flow::Go => {}
                    other => return other,
                },
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    return if self.stopping() { Flow::Stop } else { Flow::Go };
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Flow::Stop,
            }
        }
    }

    fn apply(&mut self, command: Command) -> Flow {
        match command {
            Command::Reload { items } => {
                self.load(items);
                Flow::Reloaded
            }
        }
    }

    /// Plays one scheduled file into the source's ring.
    fn play(&mut self, path: &Path) -> Flow {
        log::debug!(
            "Media scheduler {:?}: playing {}",
            self.source,
            path.display()
        );
        // The sink borrows `self`, so it is scoped to the call.
        let outcome = {
            let mut sink = |samples: &[f32]| {
                match self.drain_commands() {
                    Flow::Go => {}
                    other => return ControlFlow::Break(other),
                }
                // A due item found here joins the queue and plays when this one
                // finishes; it never interrupts. Two announcements at once is
                // the one outcome worse than one of them being a few seconds
                // late.
                self.collect_due();
                match self.feed(samples) {
                    Flow::Go => ControlFlow::Continue(()),
                    other => ControlFlow::Break(other),
                }
            };
            decode::stream_file(path, &mut sink)
        };
        match outcome {
            Ok(Some(flow)) => flow,
            Ok(None) => Flow::Go,
            // One line per unplayable file, then on to the next: a schedule
            // with a broken entry in it is not a broken scheduler. There is no
            // failure-streak backoff here (unlike the Media Player) because
            // nothing retries — the item is not due again until its next
            // occurrence, which is a minute away at the very soonest.
            Err(message) => {
                log::warn!(
                    "Media scheduler {:?}: could not play {}: {message}",
                    self.source,
                    path.display()
                );
                Flow::Go
            }
        }
    }

    /// Pushes a decoded chunk into the source's ring, pacing to the mixer.
    ///
    /// **Every wait in here must return what it was handed** — the same rule,
    /// and the same reason, as [`super::player`]: `wait` takes a command off the
    /// channel and acts on it, so the [`Flow`] it returns is the only remaining
    /// evidence that the command arrived.
    fn feed(&mut self, samples: &[f32]) -> Flow {
        let mut offset = 0;
        let mut missing_since: Option<Instant> = None;
        while offset < samples.len() {
            match self.drain_commands() {
                Flow::Go => {}
                other => return other,
            }
            // Stay only just ahead of the mixer. See `MAX_QUEUED`.
            if self
                .feeds
                .queued(&self.source)
                .is_some_and(|queued| queued >= MAX_QUEUED)
            {
                match self.wait(RING_WAIT) {
                    Flow::Go => {}
                    other => return other,
                }
                continue;
            }
            match self.feeds.push(&self.source, &samples[offset..]) {
                FeedResult::Done => return Flow::Go,
                FeedResult::Full { accepted } => {
                    offset += accepted;
                    missing_since = None;
                    if offset < samples.len() {
                        match self.wait(RING_WAIT) {
                            Flow::Go => {}
                            other => return other,
                        }
                    }
                }
                // The source is not in the mixer right now: the ordinary state
                // for the block or two between a scene switch and the engine
                // applying it. A worker whose source is really gone is stopped
                // and joined.
                FeedResult::Gone => {
                    let since = missing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= NO_RING_PATIENCE {
                        log::debug!(
                            "Media scheduler {:?}: waiting for the mixer to take this source",
                            self.source
                        );
                        missing_since = Some(Instant::now());
                    }
                    match self.wait(NO_RING_WAIT) {
                        Flow::Go => {}
                        other => return other,
                    }
                }
            }
        }
        Flow::Go
    }
}

impl Armed {
    /// Whether this item can fire at all.
    fn is_live(&self) -> bool {
        self.item.enabled && !self.item.file.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScheduleTrigger;

    fn item(file: &str, trigger: ScheduleTrigger) -> ScheduleItem {
        ScheduleItem {
            file: file.to_string(),
            enabled: true,
            trigger,
        }
    }

    /// A fixed moment to reason from, so nothing here depends on when the tests
    /// happen to run.
    fn at(hour: u32, minute: u32, second: u32) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 8,
            day: 29,
            hour,
            minute,
            second,
        }
    }

    /// A worker primed at `now`, exactly as `Scheduler::start` primes one.
    fn worker(items: Vec<ScheduleItem>, now: LocalTime) -> Worker {
        let (_, rx) = crossbeam_channel::unbounded();
        let mut worker = Worker {
            source: "Media Scheduler".into(),
            feeds: ExternalFeeds::default(),
            commands: rx,
            status: Arc::new(Mutex::new(Status::default())),
            generation: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            items: Vec::new(),
            queue: VecDeque::new(),
            last_due_check: None,
        };
        worker.load_at(items, now);
        worker
    }

    /// Runs the due check at `now`, bypassing the rate limit — which exists to
    /// keep the cost down in the feed loop, not to decide anything.
    fn check(worker: &mut Worker, now: LocalTime) {
        let unix = now.to_unix().expect("a real local time");
        worker.collect_due_at(now, unix);
    }

    /// The property the priming exists for: a worker that has just started owes
    /// nothing, however recently an item was due.
    #[test]
    fn a_freshly_loaded_schedule_fires_nothing() {
        let start = at(9, 0, 20);
        let mut worker = worker(
            vec![
                item(r"C:\chime.mp3", ScheduleTrigger::EveryMinutes { minutes: 1 }),
                item(r"C:\hour.mp3", ScheduleTrigger::Hourly { minute: 0 }),
            ],
            start,
        );
        check(&mut worker, start);
        assert!(worker.queue.is_empty(), "{:?}", worker.queue);
    }

    /// Every item is primed, so re-loading — a settings edit, a scene switch —
    /// is as quiet as starting up.
    #[test]
    fn reloading_a_schedule_fires_nothing_either() {
        let start = at(9, 30, 0);
        let mut worker = worker(
            vec![item(r"C:\chime.mp3", ScheduleTrigger::Hourly { minute: 0 })],
            start,
        );
        worker.load_at(
            vec![item(r"C:\chime.mp3", ScheduleTrigger::Hourly { minute: 0 })],
            start,
        );
        check(&mut worker, start);
        assert!(worker.queue.is_empty(), "the 09:00 item fired at 09:30");
    }

    /// An occurrence that has come round since the last look is queued, exactly
    /// once however often the check runs.
    #[test]
    fn a_due_item_is_queued_once() {
        let mut worker = worker(
            vec![item(r"C:\hour.mp3", ScheduleTrigger::Hourly { minute: 0 })],
            at(8, 59, 50),
        );
        check(&mut worker, at(8, 59, 59));
        assert!(worker.queue.is_empty(), "fired before it was due");

        check(&mut worker, at(9, 0, 1));
        assert_eq!(worker.queue.len(), 1, "the hour did not fire");

        // Again, and again later in the same minute: the occurrence has been
        // recorded, so nothing more is owed.
        check(&mut worker, at(9, 0, 2));
        check(&mut worker, at(9, 0, 29));
        assert_eq!(worker.queue.len(), 1, "fired twice for one occurrence");
    }

    /// A tick missed while another item was playing must still fire, because the
    /// occurrence has not moved — the whole reason firing is decided by
    /// comparing occurrences rather than by a timer.
    #[test]
    fn an_occurrence_missed_by_a_late_check_still_fires() {
        let mut worker = worker(
            vec![item(r"C:\hour.mp3", ScheduleTrigger::Hourly { minute: 0 })],
            at(8, 59, 0),
        );
        // The check that should have happened at 09:00:00 arrives late, but
        // inside the grace.
        check(&mut worker, at(9, 0, schedule::STALE_GRACE as u32));
        assert_eq!(worker.queue.len(), 1);
    }

    /// An occurrence the machine slept through is recorded as dealt with, not
    /// played — and not left to fire on the next check either.
    #[test]
    fn an_occurrence_that_is_too_late_is_skipped_rather_than_played() {
        let mut worker = worker(
            vec![item(r"C:\hour.mp3", ScheduleTrigger::Hourly { minute: 0 })],
            at(8, 59, 0),
        );
        // Woken twenty minutes after the hour.
        check(&mut worker, at(9, 20, 0));
        assert!(worker.queue.is_empty(), "a stale occurrence was played");

        // And it must not come back on the next look either.
        check(&mut worker, at(9, 20, 1));
        assert!(worker.queue.is_empty(), "the stale occurrence was retried");

        // The next hour is unaffected.
        check(&mut worker, at(10, 0, 1));
        assert_eq!(worker.queue.len(), 1);
    }

    /// Two items due in the same minute both play, in order, rather than one
    /// replacing the other.
    #[test]
    fn items_due_together_are_both_queued() {
        let mut worker = worker(
            vec![
                item(
                    r"C:\quarter.mp3",
                    ScheduleTrigger::EveryMinutes { minutes: 15 },
                ),
                item(r"C:\hour.mp3", ScheduleTrigger::Hourly { minute: 0 }),
            ],
            at(8, 59, 0),
        );
        check(&mut worker, at(9, 0, 1));
        assert_eq!(worker.queue.len(), 2, "{:?}", worker.queue);
    }

    /// The user's own schedule, walked over a whole day: each hour's file plays
    /// once, at its hour and again twelve hours later.
    #[test]
    fn a_twelve_hour_chime_plays_the_right_file_every_hour() {
        let items: Vec<ScheduleItem> = (1..=12u32)
            .flat_map(|chime| {
                [chime, (chime + 12) % 24].map(move |hour| {
                    item(
                        &format!(r"O:\hours\{chime:02}.mp3"),
                        ScheduleTrigger::DailyAt { hour, minute: 0 },
                    )
                })
            })
            .collect();
        assert_eq!(items.len(), 24);

        let mut worker = worker(items, at(0, 0, 30));
        let mut heard: Vec<(u32, String)> = Vec::new();
        // Starting at 01:00 — midnight had already gone by when the worker was
        // primed above, which is exactly the behaviour priming exists for.
        for hour in 1..24 {
            check(&mut worker, at(hour, 0, 1));
            while let Some(file) = worker.queue.pop_front() {
                heard.push((hour, track_title(&file)));
            }
        }

        assert_eq!(heard.len(), 23, "one chime an hour: {heard:?}");
        assert_eq!(heard[0], (1, "01".to_string()));
        assert_eq!(heard[11], (12, "12".to_string()));
        // The half a twelve-hour clock exists for.
        assert_eq!(heard[12], (13, "01".to_string()));
        assert_eq!(heard[22], (23, "11".to_string()));

        // And midnight, on the next day round, plays 12.
        check(&mut worker, at(0, 0, 1).next_day());
        assert_eq!(
            worker.queue.pop_front().as_deref().map(track_title),
            Some("12".to_string())
        );
    }

    /// A disabled item and one with no file are both inert, and a scheduler made
    /// only of those has nothing to say about what is next.
    #[test]
    fn disabled_and_unset_items_never_fire() {
        let mut worker = worker(
            vec![
                ScheduleItem {
                    enabled: false,
                    ..item(r"C:\off.mp3", ScheduleTrigger::EveryMinutes { minutes: 1 })
                },
                item("   ", ScheduleTrigger::EveryMinutes { minutes: 1 }),
            ],
            at(8, 59, 0),
        );
        check(&mut worker, at(9, 0, 1));
        assert!(worker.queue.is_empty());
        assert!(!worker.items.iter().any(Armed::is_live));
        assert_eq!(
            worker.next_due(at(9, 0, 1)).map(|(i, _)| i.file.clone()),
            None
        );
    }

    /// The check is asked for far more often than a minute-resolution schedule
    /// can answer, so it must cost nothing between times.
    #[test]
    fn the_due_check_is_rate_limited() {
        let mut worker = worker(
            vec![item(
                r"C:\chime.mp3",
                ScheduleTrigger::EveryMinutes { minutes: 1 },
            )],
            LocalTime::now(),
        );
        worker.collect_due();
        let first = worker.last_due_check;
        worker.collect_due();
        assert_eq!(worker.last_due_check, first, "it looked again too soon");
    }

    /// A queue that has fallen behind sheds its oldest rather than growing.
    #[test]
    fn the_queue_is_capped() {
        let mut worker = worker(
            vec![item(r"C:\hour.mp3", ScheduleTrigger::Hourly { minute: 0 })],
            at(8, 59, 0),
        );
        for n in 0..MAX_QUEUED_ITEMS {
            worker.queue.push_back(PathBuf::from(format!("old{n}.mp3")));
        }
        check(&mut worker, at(9, 0, 1));

        assert_eq!(worker.queue.len(), MAX_QUEUED_ITEMS);
        assert_eq!(
            worker.queue.back(),
            Some(&PathBuf::from(r"C:\hour.mp3")),
            "the newly due item must be the one kept"
        );
        assert_ne!(worker.queue.front(), Some(&PathBuf::from("old0.mp3")));
    }

    /// An idle worker must not spin, and must not sleep past the next item.
    #[test]
    fn the_idle_sleep_is_bounded_at_both_ends() {
        let empty = worker(Vec::new(), LocalTime::now());
        assert_eq!(empty.idle_sleep(), MAX_SLEEP, "nothing to wait for");

        let soon = worker(
            vec![item(
                r"C:\chime.mp3",
                ScheduleTrigger::EveryMinutes { minutes: 1 },
            )],
            LocalTime::now(),
        );
        let sleep = soon.idle_sleep();
        assert!(sleep <= Duration::from_secs(60), "{sleep:?}");
        assert!(sleep <= MAX_SLEEP);
    }

    /// The label has to name the item that is coming, not just say one is.
    #[test]
    fn the_next_item_is_named_with_its_time() {
        let mut worker = worker(
            vec![item(
                r"C:\nine.mp3",
                ScheduleTrigger::DailyAt { hour: 9, minute: 0 },
            )],
            LocalTime::now(),
        );
        worker.set_status(SchedulerState::Waiting, None);
        let status = crate::audio::device::lock_recovering(&worker.status, "test").clone();
        assert_eq!(status.next, Some(("nine".to_string(), "09:00".to_string())));
    }
}
