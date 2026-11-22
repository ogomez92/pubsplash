//! One media player: a thread that decodes a folder of music into a source's
//! ring, and the handle the UI drives it with.
//!
//! ## Why a thread per source
//!
//! Feeding [`ExternalFeeds`] is paced by the mixer's drain rate — pushing a
//! track takes as long as the track lasts — so it cannot happen on the UI
//! thread, and it cannot happen on the audio thread either: this opens files,
//! allocates, and runs a decoder. It is the same shape as
//! [`crate::ui::cue_feed`], one worker per source, except that this one is
//! never idle while its scene is active.
//!
//! ## Decoding
//!
//! A track is decoded *as it plays*, one packet at a time, and never held in
//! memory whole. That part is [`super::decode::stream_file`], shared with the
//! Media Scheduler; its header has the reasoning. What is left here is the
//! player around it — the playlist, the transport, and the pacing.
//!
//! ## Waiting
//!
//! Every wait in here is a `recv_timeout` on the command channel rather than a
//! sleep, so pause, skip and shutdown are acted on the moment they arrive
//! however long the timeout is. The timeouts themselves exist only for the one
//! thing that has no event behind it: the ring filling up, which drains at the
//! mixer's own pace.

use crate::audio::{ExternalFeeds, FeedResult};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Playlist, decode, scan_folder, track_title};

/// How far ahead of the mixer this worker is allowed to decode, in samples.
///
/// The source's ring holds a whole second, and filling it would be the obvious
/// thing to do — but everything in that ring **will be heard**, whatever the
/// user presses next. Pause and next track would then take up to a second to
/// take effect, on a control whose whole job is to be immediate. A quarter of a
/// second is still ten times the length of a decoded packet, so a disk seek or
/// a slow file cannot starve the mixer, and it is short enough that a skip
/// sounds instant.
const MAX_QUEUED: usize =
    (crate::audio::mixer::SAMPLE_RATE as usize / 4) * crate::audio::mixer::CHANNELS;

/// How long to wait for a command when the only other thing to wait for is the
/// ring draining. A fraction of [`MAX_QUEUED`], so topping the ring back up is
/// never late.
const RING_WAIT: Duration = Duration::from_millis(50);

/// How long to wait between attempts when the source's ring is not there yet.
/// A scene switch sends the engine its new sources and starts these workers in
/// the same breath, so a worker can beat its own ring into existence by a block
/// or two.
const NO_RING_WAIT: Duration = Duration::from_millis(100);

/// How long a missing ring is tolerated before it is worth a log line.
const NO_RING_PATIENCE: Duration = Duration::from_secs(3);

/// How long to wait, when there is nothing to play, before looking at the
/// folder again — the case where the user is filling it while Pubsplash runs,
/// or a drive that went away comes back.
///
/// This is a poll, and the one left in the player: with a list to play, the
/// folder is read only when the list runs out, which is an event the worker
/// sees for itself. A watcher on the folder would answer the empty case without
/// polling, but it would be a new dependency and a recursive watch over a whole
/// music library for the one state where the player has nothing else to do.
const EMPTY_RESCAN: Duration = Duration::from_secs(15);

/// How long to wait after a run of unplayable files before trying again.
/// Without it, a folder Pubsplash cannot read would spin this thread at the
/// speed of `File::open`.
const FAILURE_BACKOFF: Duration = Duration::from_secs(5);

/// How many files in a row may fail before that backoff.
///
/// Capped well below the size of a real library on purpose. The failure this
/// guards against is not one bad file — that is skipped and taken out of the
/// list — but the whole folder becoming unreadable at once, which is what a
/// disconnected drive looks like. Walking a thousand paths to find that out
/// would take a thousand log lines to say it.
const FAILURE_STREAK: usize = 10;

/// What a media player is doing, for the source's label in the Sources list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PlaybackState {
    /// No folder is set: the source is configured but has nothing to play.
    #[default]
    NoFolder,
    /// The folder holds no file this build can decode.
    NoFiles,
    Playing,
    Paused,
}

/// One row of a player's list: the file, and what the user is told it is
/// called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub path: PathBuf,
    pub title: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub state: PlaybackState,
    /// The track playing (or paused), without its extension.
    pub track: Option<String>,
    /// The whole list, in the order it plays. Shared rather than copied: a
    /// library folder is thousands of entries, and the UI asks for the status
    /// on every keypress that drives the player. A new `Arc` is built only
    /// when the entries change, which is also what makes comparing two
    /// statuses cheap — equal `Arc`s compare by pointer first.
    pub queue: Arc<Vec<QueueEntry>>,
    /// Which row of `queue` is playing. `None` before anything has played, and
    /// after the playing entry has been removed from the list (it plays on).
    pub playing: Option<usize>,
    /// Changes whenever the entries in `queue` change, and only then — a
    /// track starting moves `playing` without touching it. What lets the list
    /// on screen tell "relabel two rows" from "rebuild". Unique across every
    /// player in the process.
    pub revision: u64,
    /// The track "next track" would move to, for its announcement. `None` at
    /// the end of the list, where the folder has not been read again yet.
    pub next_track: Option<String>,
    /// The track "previous track" would move to, for its announcement. The
    /// track playing now when it is the first in the list, since that is what
    /// a previous then does — starts it again.
    pub previous: Option<String>,
}

impl Status {
    /// The track a skip would move to.
    pub fn next(&self) -> Option<&str> {
        self.next_track.as_deref()
    }
}

/// What the UI can ask a running player to do.
pub enum Command {
    PlayPause,
    /// Abandon the current track and start the next one.
    Next,
    /// Abandon the current track and go back to the one above it in the list;
    /// from the first, start it again.
    Previous,
    /// Abandon the current track and play this file, which joins the list just
    /// below the track it interrupted. The file does not have to be in the
    /// folder, or in any folder this source knows about.
    PlayFile(PathBuf),
    /// Abandon the current track and play this entry of the list, carrying on
    /// from the one below it. `path` is what the user saw in that row, so a
    /// list that moved under them still plays what they chose.
    PlayAt { index: usize, path: PathBuf },
    /// Take this entry out of the list. The file is untouched, and the folder
    /// brings it back when the list runs out and is read again.
    Remove { index: usize, path: PathBuf },
    /// The source's settings changed. A new folder is read and replaces the
    /// list; a shuffle change reorders what has not played yet.
    Reload { folder: String, shuffle: bool },
}

/// A running media player. Dropping the handle does **not** stop the thread —
/// use [`Player::stop`], which is what guarantees the old worker is gone before
/// a new one can be pushing into the same ring.
pub struct Player {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// What this worker was started with, so the UI can tell whether a config
    /// edit actually changed anything.
    pub folder: String,
    pub shuffle: bool,
}

impl Player {
    pub fn start(
        source_name: String,
        feeds: ExternalFeeds,
        folder: String,
        shuffle: bool,
        generation: Arc<AtomicU64>,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let status = Arc::new(Mutex::new(Status::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("media-player".into())
            .spawn({
                let mut worker = Worker {
                    source: source_name,
                    feeds,
                    commands: rx,
                    status: status.clone(),
                    generation,
                    stop: stop.clone(),
                    folder: folder.clone(),
                    shuffle,
                    paused: false,
                    playlist: Playlist::new(Vec::new(), shuffle),
                    pending: None,
                    last_played: None,
                    last_count: None,
                    queue: Arc::default(),
                    queue_revision: 0,
                    failures: 0,
                };
                move || worker.run()
            })
            .ok();
        Self {
            commands: tx,
            status,
            stop,
            thread,
            folder,
            shuffle,
        }
    }

    pub fn status(&self) -> Status {
        crate::audio::device::lock_recovering(&self.status, "Media player status").clone()
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// The worker's thread id, so a test can tell "this player is still the one
    /// that was playing" from "it was stopped and started again".
    #[cfg(test)]
    pub fn thread_id(&self) -> Option<std::thread::ThreadId> {
        self.thread.as_ref().map(|t| t.thread().id())
    }

    /// Stops the worker and waits for it to let go of the ring.
    ///
    /// The join is what makes this safe rather than tidy: a detached worker
    /// would keep pushing into `ExternalFeeds` by source *name*, and a scene
    /// with a media player of the same name would find a stranger's audio in
    /// its ring. Every wait in the worker is a `recv_timeout`, and dropping the
    /// sender ends all of them at once, so this returns in about the time it
    /// takes to notice — never the length of a track.
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
    /// Stop the current track and pick another.
    NextTrack,
    /// The playlist has been replaced; stop the current track.
    Reloaded,
    /// The worker is finished.
    Stop,
}

struct Worker {
    source: String,
    feeds: ExternalFeeds,
    commands: Receiver<Command>,
    status: Arc<Mutex<Status>>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    folder: String,
    shuffle: bool,
    paused: bool,
    playlist: Playlist,
    /// A track the playlist has already been moved to — by Open file, by
    /// playing an entry, or by Previous — to be played by the next pass round
    /// the loop. Taken there, so it survives being set from anywhere a command
    /// is drained.
    pending: Option<PathBuf>,
    /// The track that played last, so a fresh shuffle does not open with it.
    last_played: Option<PathBuf>,
    /// How many files the last read of the folder found, so an empty folder
    /// re-read every [`EMPTY_RESCAN`] logs once rather than four times a minute.
    last_count: Option<usize>,
    /// The list as last published, rebuilt only when the playlist's revision
    /// moves. See [`Status::queue`].
    queue: Arc<Vec<QueueEntry>>,
    queue_revision: u64,
    /// Tracks that failed to produce any audio since the last one that did.
    failures: usize,
}

impl Worker {
    /// The waits in here test only for [`Flow::Stop`], which is safe *here* and
    /// nowhere else: every one of them is followed by `continue`, and the top of
    /// this loop is where a skip or a reload is acted on anyway. Inside
    /// [`Worker::feed`] the same shape drops the command on the floor — see the
    /// note there.
    fn run(&mut self) {
        loop {
            if self.stopping() {
                return;
            }
            // A track the user chose outright is played before anything else —
            // including when the folder is empty, or not set at all, which is a
            // perfectly reasonable way to use this source with Open file.
            if let Some(path) = self.pending.take() {
                if self.play_track(path) == Flow::Stop {
                    return;
                }
                continue;
            }
            if self.paused {
                // Paused with nothing to do: the wait ends the moment a command
                // arrives, so this costs one wakeup a second.
                if self.wait(Duration::from_secs(1)) == Flow::Stop {
                    return;
                }
                continue;
            }
            // A run of files that produced no audio at all: back off rather
            // than reopening them as fast as the disk allows. Each one has
            // already been taken out of the list, so the list runs out and the
            // folder is read again — the usual cause is that it is not there
            // any more.
            if self.failures >= FAILURE_STREAK.min(self.playlist.len().max(1)) {
                self.failures = 0;
                log::warn!(
                    "Media player {:?}: nothing in {} could be played; trying again shortly",
                    self.source,
                    self.folder
                );
                if self.wait(FAILURE_BACKOFF) == Flow::Stop {
                    return;
                }
                continue;
            }
            // The end of the list is the one moment the folder is read again:
            // songs added to it since join, songs removed from the list come
            // back, and a shuffled list gets a fresh order.
            if self.playlist.is_exhausted() {
                self.refill();
                if self.playlist.is_empty() {
                    let state = if self.folder.trim().is_empty() {
                        PlaybackState::NoFolder
                    } else {
                        PlaybackState::NoFiles
                    };
                    self.set_status(state, None);
                    if self.wait(EMPTY_RESCAN) == Flow::Stop {
                        return;
                    }
                    continue;
                }
            }
            let Some(track) = self.playlist.next() else {
                continue;
            };
            if self.play_track(track) == Flow::Stop {
                return;
            }
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Reads the folder and makes what it holds the new list.
    fn refill(&mut self) {
        let folder = self.folder.trim();
        let files = if folder.is_empty() {
            Vec::new()
        } else {
            scan_folder(Path::new(folder))
        };
        if self.last_count != Some(files.len()) {
            log::info!(
                "Media player {:?}: {} playable files in {:?}",
                self.source,
                files.len(),
                folder
            );
            self.last_count = Some(files.len());
        }
        let last = self.last_played.take();
        self.playlist.refill(files, last.as_deref());
        self.last_played = last;
    }

    /// Plays one track the playlist has already been moved to.
    fn play_track(&mut self, path: PathBuf) -> Flow {
        self.set_status(PlaybackState::Playing, Some(track_title(&path)));
        self.last_played = Some(path.clone());
        self.play(&path)
    }

    /// Publishes what this player is doing. The list, and the tracks next and
    /// previous would move to, are read off the playlist here rather than
    /// passed in, so every path that changes the status refreshes them.
    fn set_status(&mut self, state: PlaybackState, track: Option<String>) {
        if self.queue_revision != self.playlist.revision() {
            self.queue = Arc::new(
                self.playlist
                    .entries()
                    .iter()
                    .map(|path| QueueEntry {
                        path: path.clone(),
                        title: track_title(path),
                    })
                    .collect(),
            );
            self.queue_revision = self.playlist.revision();
        }
        let next = Status {
            state,
            track,
            queue: self.queue.clone(),
            playing: self.playlist.playing(),
            revision: self.queue_revision,
            next_track: self.playlist.peek().map(|path| track_title(path)),
            previous: self.playlist.peek_previous().map(|path| track_title(path)),
        };
        let mut status =
            crate::audio::device::lock_recovering(&self.status, "Media player status");
        if *status == next {
            return;
        }
        *status = next;
        drop(status);
        // The pump notices the counter moved and re-derives the source's label
        // and the strip's list; see `ui::media`.
        self.generation.fetch_add(1, Ordering::Relaxed);
        wxdragon::wake_up_idle();
    }

    /// Republishes the current state after an edit to the list, which changes
    /// the status without a track starting or stopping.
    fn publish(&mut self) {
        let current = self.status();
        self.set_status(current.state, current.track);
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
            // The handle is gone, which only happens through `Player::stop`.
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
            Command::PlayPause => {
                self.paused = !self.paused;
                Flow::Go
            }
            Command::Next => {
                // Skipping while paused starts playing again: the user asked
                // for a different track, not for a different silence.
                self.paused = false;
                Flow::NextTrack
            }
            // The same skip, to a file of the user's choosing. It joins the
            // list where it plays, so the list carries on below it.
            Command::PlayFile(path) => {
                self.paused = false;
                self.playlist.insert_and_play(path.clone());
                self.pending = Some(path);
                Flow::NextTrack
            }
            Command::PlayAt { index, path } => match self.playlist.find(index, &path) {
                Some(index) => {
                    self.paused = false;
                    self.playlist.play_at(index);
                    self.pending = Some(path);
                    Flow::NextTrack
                }
                // Gone from the list since the user saw it — the list ran out
                // and was read again. Nothing they asked for is left to play.
                None => Flow::Go,
            },
            // Never interrupts anything: removing the playing entry leaves it
            // playing to its end, which on air is the only safe answer.
            Command::Remove { index, path } => {
                if let Some(index) = self.playlist.find(index, &path) {
                    self.playlist.remove(index);
                }
                self.publish();
                Flow::Go
            }
            // Played through `pending`, because the playlist has already moved
            // back: taking it from `playlist.next()` as well would move it on.
            Command::Previous => match self.playlist.previous() {
                Some(path) => {
                    self.paused = false;
                    self.pending = Some(path);
                    Flow::NextTrack
                }
                // Nothing has played yet, so there is nothing to go back to,
                // and the track that is about to start is left to start.
                None => Flow::Go,
            },
            Command::Reload { folder, shuffle } => {
                if folder != self.folder {
                    self.folder = folder;
                    self.shuffle = shuffle;
                    // An empty list is an exhausted one, so the loop reads the
                    // new folder the moment the current track is abandoned.
                    self.playlist = Playlist::new(Vec::new(), shuffle);
                    self.last_count = None;
                    return Flow::Reloaded;
                }
                if shuffle != self.shuffle {
                    self.shuffle = shuffle;
                    self.playlist.set_shuffle(shuffle);
                    self.publish();
                }
                Flow::Go
            }
        }
    }

    /// Decodes one file into the source's ring, in step with the mixer.
    fn play(&mut self, path: &Path) -> Flow {
        log::debug!("Media player {:?}: playing {}", self.source, path.display());
        match self.decode(path) {
            Ok(flow) => flow,
            Err(message) => {
                // One line per unreadable file, then on to the next: a folder
                // with a broken file in it is not a broken media player. The
                // file leaves the list, so the user does not keep finding it.
                log::warn!(
                    "Media player {:?}: could not play {}: {message}",
                    self.source,
                    path.display()
                );
                self.failures += 1;
                if let Some(index) = self.playlist.playing()
                    && self.playlist.entries().get(index).is_some_and(|entry| entry == path)
                {
                    self.playlist.remove(index);
                    self.publish();
                }
                Flow::Go
            }
        }
    }

    /// Runs one file through the shared streaming decoder, answering commands
    /// between packets.
    ///
    /// The decoding itself is [`decode::stream_file`], which the Media Scheduler
    /// shares. What stays here is everything that makes this a *player*: the
    /// command drain and the pause check between packets, and the [`Flow`] a
    /// skip produces travelling back out through the sink.
    fn decode(&mut self, path: &Path) -> Result<Flow, String> {
        // The sink borrows `self` for as long as it lives, so it is scoped to
        // the call rather than left in scope for the `self.failures` write below.
        let interrupted = {
            let mut sink = |samples: &[f32]| {
                // Commands first, so skip and pause are answered between packets
                // rather than at the end of the track.
                match self.drain_commands() {
                    Flow::Go => {}
                    other => return ControlFlow::Break(other),
                }
                if self.paused {
                    match self.wait_while_paused() {
                        Flow::Go => {}
                        other => return ControlFlow::Break(other),
                    }
                }
                match self.feed(samples) {
                    Flow::Go => ControlFlow::Continue(()),
                    other => ControlFlow::Break(other),
                }
            };
            decode::stream_file(path, &mut sink)?
        };
        match interrupted {
            Some(flow) => Ok(flow),
            None => {
                self.failures = 0;
                Ok(Flow::Go)
            }
        }
    }

    /// Holds the current track where it is until play resumes. The ring drains
    /// into silence on its own, which is what makes pause immediate.
    fn wait_while_paused(&mut self) -> Flow {
        let track = self.status().track;
        self.set_status(PlaybackState::Paused, track.clone());
        while self.paused {
            match self.wait(Duration::from_secs(1)) {
                Flow::Go => {}
                other => return other,
            }
        }
        self.set_status(PlaybackState::Playing, track);
        Flow::Go
    }

    fn status(&self) -> Status {
        crate::audio::device::lock_recovering(&self.status, "Media player status").clone()
    }

    /// Pushes a decoded chunk into the source's ring, pacing to the mixer.
    ///
    /// **Every wait in here must return what it was handed.** [`Worker::wait`]
    /// takes a command off the channel and acts on it, so a flow it produces is
    /// the only remaining evidence that the command arrived — a caller that
    /// tests it against one variant and carries on has silently eaten a
    /// keypress. This loop is where a media player spends nearly all of its
    /// life (it keeps a quarter of a second queued and waits out the rest), so
    /// nearly every skip the user presses is answered here, and testing only
    /// for [`Flow::Stop`] is what made next track do nothing at all.
    fn feed(&mut self, samples: &[f32]) -> Flow {
        let mut offset = 0;
        let mut missing_since: Option<Instant> = None;
        while offset < samples.len() {
            match self.drain_commands() {
                Flow::Go => {}
                other => return other,
            }
            if self.paused {
                match self.wait_while_paused() {
                    Flow::Go => {}
                    other => return other,
                }
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
                // The source is not in the mixer right now. Waiting rather than
                // dropping the audio: this is the ordinary state for the block
                // or two between a scene switch and the engine applying it, and
                // a worker whose source is really gone is stopped and joined.
                FeedResult::Gone => {
                    let since = missing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= NO_RING_PATIENCE {
                        log::debug!(
                            "Media player {:?}: waiting for the mixer to take this source",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder holding one WAV of a tone, at a rate that is not the engine's
    /// so the resampling path is exercised too.
    fn folder_with_a_tone(name: &str) -> std::path::PathBuf {
        use hound::{SampleFormat, WavSpec, WavWriter};
        let folder = std::env::temp_dir().join(format!("pubsplash-player-{name}"));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let rate = 44_100;
        let spec = WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(folder.join("tone.wav"), spec).unwrap();
        // Two seconds, so the ring fills and the player is still mid-track when
        // the test looks.
        for frame in 0..rate * 2 {
            let phase = frame as f32 / rate as f32 * 440.0 * std::f32::consts::TAU;
            let sample = (phase.sin() * 16_384.0) as i16;
            writer.write_sample(sample).unwrap();
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        folder
    }

    /// A folder of several named tracks, so a test can tell which one is
    /// playing. Long enough that nothing reaches its end on its own.
    fn folder_of_tracks(name: &str, tracks: &[&str]) -> std::path::PathBuf {
        use hound::{SampleFormat, WavSpec, WavWriter};
        let folder = std::env::temp_dir().join(format!("pubsplash-player-{name}"));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let rate = 48_000;
        for track in tracks {
            let spec = WavSpec {
                channels: 2,
                sample_rate: rate,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            };
            let mut writer =
                WavWriter::create(folder.join(format!("{track}.wav")), spec).unwrap();
            for frame in 0..rate * 30 {
                let phase = frame as f32 / rate as f32 * 440.0 * std::f32::consts::TAU;
                let sample = (phase.sin() * 16_384.0) as i16;
                writer.write_sample(sample).unwrap();
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }
        folder
    }

    /// Skipping moves to the next track rather than staying where it was.
    #[test]
    fn next_track_starts_the_next_track() {
        let folder = folder_of_tracks("next", &["a", "b"]);
        let feeds = ExternalFeeds::default();
        let mut ring =
            feeds.park_for_test("Media Player", crate::audio::mixer::SAMPLE_RATE as usize);
        let player = Player::start(
            "Media Player".to_string(),
            feeds,
            folder.to_string_lossy().into_owned(),
            false,
            Arc::new(AtomicU64::new(0)),
        );

        // Drain like the mixer would, so the worker is doing real work rather
        // than parked against a full ring.
        let drain = Arc::new(AtomicBool::new(false));
        let drainer = std::thread::spawn({
            let drain = drain.clone();
            move || {
                while !drain.load(Ordering::Relaxed) {
                    let slots = ring.slots();
                    if slots > 0 && let Ok(chunk) = ring.read_chunk(slots) {
                        chunk.commit_all();
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while player.status().track.as_deref() != Some("a") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(player.status().track.as_deref(), Some("a"), "first track");
        // What the skip about to be pressed will be announced as.
        assert_eq!(player.status().next(), Some("b"));

        player.send(Command::Next);

        let deadline = Instant::now() + Duration::from_secs(5);
        while player.status().track.as_deref() == Some("a") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let after = player.status();
        drain.store(true, Ordering::Relaxed);
        let _ = drainer.join();
        player.stop();
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(after.state, PlaybackState::Playing);
        assert_eq!(after.track.as_deref(), Some("b"), "skip stayed put");
    }

    /// Previous goes back to the track before, and says so ahead of time.
    #[test]
    fn previous_track_goes_back() {
        let folder = folder_of_tracks("previous", &["a", "b", "c"]);
        let feeds = ExternalFeeds::default();
        let mut ring =
            feeds.park_for_test("Media Player", crate::audio::mixer::SAMPLE_RATE as usize);
        let player = Player::start(
            "Media Player".to_string(),
            feeds,
            folder.to_string_lossy().into_owned(),
            false,
            Arc::new(AtomicU64::new(0)),
        );
        let drain = Arc::new(AtomicBool::new(false));
        let drainer = std::thread::spawn({
            let drain = drain.clone();
            move || {
                while !drain.load(Ordering::Relaxed) {
                    let slots = ring.slots();
                    if slots > 0 && let Ok(chunk) = ring.read_chunk(slots) {
                        chunk.commit_all();
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });
        let wait_for = |what: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while player.status().track.as_deref() != Some(what) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            player.status()
        };

        let first = wait_for("a");
        player.send(Command::Next);
        let second = wait_for("b");
        player.send(Command::Previous);
        let back = wait_for("a");
        player.send(Command::Next);
        let forward = wait_for("b");

        drain.store(true, Ordering::Relaxed);
        let _ = drainer.join();
        player.stop();
        let _ = std::fs::remove_dir_all(&folder);

        // On the first track, previous would restart it.
        assert_eq!(first.previous.as_deref(), Some("a"));
        assert_eq!(second.previous.as_deref(), Some("a"));
        assert_eq!(back.track.as_deref(), Some("a"), "previous stayed put");
        assert_eq!(back.next(), Some("b"), "the way forward is where it came from");
        assert_eq!(forward.track.as_deref(), Some("b"));
        assert_eq!(forward.next(), Some("c"));
    }

    /// Open file: the chosen track interrupts what was playing, and the folder
    /// picks up afterwards as though it had never happened.
    #[test]
    fn an_opened_file_plays_and_leaves_the_folder_where_it_was() {
        let folder = folder_of_tracks("open", &["a", "b"]);
        // Deliberately somewhere else: the point of the feature is that the file
        // does not have to be in the source's folder.
        let elsewhere = folder_of_tracks("open-elsewhere", &["chosen"]);
        let feeds = ExternalFeeds::default();
        let mut ring =
            feeds.park_for_test("Media Player", crate::audio::mixer::SAMPLE_RATE as usize);
        let player = Player::start(
            "Media Player".to_string(),
            feeds,
            folder.to_string_lossy().into_owned(),
            false,
            Arc::new(AtomicU64::new(0)),
        );

        let drain = Arc::new(AtomicBool::new(false));
        let drainer = std::thread::spawn({
            let drain = drain.clone();
            move || {
                while !drain.load(Ordering::Relaxed) {
                    let slots = ring.slots();
                    if slots > 0 && let Ok(chunk) = ring.read_chunk(slots) {
                        chunk.commit_all();
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });
        let wait_for = |what: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while player.status().track.as_deref() != Some(what) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            player.status()
        };

        assert_eq!(wait_for("a").track.as_deref(), Some("a"), "first track");
        player.send(Command::PlayFile(elsewhere.join("chosen.wav")));
        let playing = wait_for("chosen");

        // And on from there: the folder resumes with the track it was up to,
        // not with a reshuffled or restarted list.
        player.send(Command::Next);
        let after = wait_for("b");

        drain.store(true, Ordering::Relaxed);
        let _ = drainer.join();
        player.stop();
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&elsewhere);

        assert_eq!(playing.track.as_deref(), Some("chosen"));
        assert_eq!(playing.state, PlaybackState::Playing);
        // The folder never stopped being the folder, so a skip out of the
        // opened file is announced as — and plays — the track that was next.
        assert_eq!(playing.next(), Some("b"));
        assert_eq!(after.track.as_deref(), Some("b"));
    }

    /// The whole path in one: a file on disk is found, decoded, resampled to
    /// the engine's rate and pushed into the source's ring.
    #[test]
    fn a_folder_of_music_reaches_the_mixer() {
        let folder = folder_with_a_tone("plays");
        let feeds = ExternalFeeds::default();
        let mut ring = feeds.park_for_test("Media Player", crate::audio::mixer::SAMPLE_RATE as usize);
        let player = Player::start(
            "Media Player".to_string(),
            feeds,
            folder.to_string_lossy().into_owned(),
            true,
            Arc::new(AtomicU64::new(0)),
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while ring.slots() < 4_800 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let available = ring.slots();
        let status = player.status();
        player.stop();
        let _ = std::fs::remove_dir_all(&folder);

        assert!(available >= 4_800, "only {available} samples arrived");
        let chunk = ring.read_chunk(4_800).unwrap();
        let (first, _) = chunk.as_slices();
        let peak = first.iter().fold(0f32, |max, s| max.max(s.abs()));
        assert!(peak > 0.4, "the tone arrived silent (peak {peak})");
        assert_eq!(status.state, PlaybackState::Playing);
        assert_eq!(status.track.as_deref(), Some("tone"));
    }

    /// Pausing stops the audio, and the ring drains rather than being refilled.
    #[test]
    fn pausing_stops_the_music() {
        let folder = folder_with_a_tone("pauses");
        let feeds = ExternalFeeds::default();
        let mut ring = feeds.park_for_test("Media Player", crate::audio::mixer::SAMPLE_RATE as usize);
        let player = Player::start(
            "Media Player".to_string(),
            feeds,
            folder.to_string_lossy().into_owned(),
            true,
            Arc::new(AtomicU64::new(0)),
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while ring.slots() < 4_800 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        player.send(Command::PlayPause);
        let paused_by = Instant::now() + Duration::from_secs(2);
        while player.status().state != PlaybackState::Paused && Instant::now() < paused_by {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(player.status().state, PlaybackState::Paused);

        // Drain what was already decoded; nothing more may arrive.
        let queued = ring.slots();
        let chunk = ring.read_chunk(queued).unwrap();
        chunk.commit_all();
        std::thread::sleep(Duration::from_millis(300));
        let after = ring.slots();
        player.stop();
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(after, 0, "a paused player kept feeding the mixer");
    }

    /// A player whose source has no ring at all: nothing consumes what it
    /// decodes, which is the state a scene switch leaves it in for a block or
    /// two. It must wait rather than spin, and it must still stop on request.
    #[test]
    fn a_player_with_nowhere_to_feed_still_stops() {
        let player = Player::start(
            "Media Player".to_string(),
            ExternalFeeds::default(),
            String::new(),
            true,
            Arc::new(AtomicU64::new(0)),
        );
        // No folder, so it settles into the "nothing to play" wait.
        let started = Instant::now();
        player.stop();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop must not wait out the rescan timer"
        );
    }

    #[test]
    fn an_unset_folder_reports_itself_rather_than_an_error() {
        let player = Player::start(
            "Media Player".to_string(),
            ExternalFeeds::default(),
            String::new(),
            true,
            Arc::new(AtomicU64::new(0)),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && player.status().state != PlaybackState::NoFolder {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(player.status().state, PlaybackState::NoFolder);
        assert_eq!(player.status().track, None);
        player.stop();
    }

    /// A player on `folder`, with a thread draining its ring like the mixer
    /// would. The returned closure stops the drainer and the player.
    fn running(folder: &Path) -> (Player, impl FnOnce(Player)) {
        let feeds = ExternalFeeds::default();
        let mut ring =
            feeds.park_for_test("Media Player", crate::audio::mixer::SAMPLE_RATE as usize);
        let player = Player::start(
            "Media Player".to_string(),
            feeds,
            folder.to_string_lossy().into_owned(),
            false,
            Arc::new(AtomicU64::new(0)),
        );
        let drain = Arc::new(AtomicBool::new(false));
        let drainer = std::thread::spawn({
            let drain = drain.clone();
            move || {
                while !drain.load(Ordering::Relaxed) {
                    let slots = ring.slots();
                    if slots > 0 && let Ok(chunk) = ring.read_chunk(slots) {
                        chunk.commit_all();
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });
        let finish = move |player: Player| {
            drain.store(true, Ordering::Relaxed);
            let _ = drainer.join();
            player.stop();
        };
        (player, finish)
    }

    fn wait_until(player: &Player, done: impl Fn(&Status) -> bool) -> Status {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = player.status();
            if done(&status) || Instant::now() >= deadline {
                return status;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn titles(status: &Status) -> Vec<&str> {
        status.queue.iter().map(|entry| entry.title.as_str()).collect()
    }

    /// The whole list is published in playing order, with the playing row.
    #[test]
    fn the_whole_list_is_published() {
        let folder = folder_of_tracks("list", &["a", "b", "c"]);
        let (player, finish) = running(&folder);
        let status = wait_until(&player, |s| s.track.as_deref() == Some("a"));
        finish(player);
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(titles(&status), vec!["a", "b", "c"]);
        assert_eq!(status.playing, Some(0));
        assert_eq!(status.next(), Some("b"));
    }

    /// Enter on a row: that entry plays, and the list carries on below it.
    #[test]
    fn an_entry_chosen_from_the_list_plays() {
        let folder = folder_of_tracks("play-at", &["a", "b", "c"]);
        let (player, finish) = running(&folder);
        wait_until(&player, |s| s.track.as_deref() == Some("a"));

        player.send(Command::PlayAt {
            index: 2,
            path: folder.join("c.wav"),
        });
        let chosen = wait_until(&player, |s| s.track.as_deref() == Some("c"));
        // A row whose index has gone stale is found by its path.
        player.send(Command::PlayAt {
            index: 0,
            path: folder.join("b.wav"),
        });
        let found = wait_until(&player, |s| s.track.as_deref() == Some("b"));
        finish(player);
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(chosen.track.as_deref(), Some("c"));
        assert_eq!(chosen.playing, Some(2));
        assert_eq!(found.playing, Some(1));
        assert_eq!(found.next(), Some("c"));
    }

    /// Delete takes an entry out of the list without touching the file or
    /// interrupting what is playing; when the list runs out the folder is read
    /// again and the entry is back.
    #[test]
    fn a_removed_entry_comes_back_when_the_list_is_read_again() {
        let folder = folder_of_tracks("remove", &["a", "b", "c"]);
        let (player, finish) = running(&folder);
        let first = wait_until(&player, |s| s.track.as_deref() == Some("a"));

        player.send(Command::Remove {
            index: 1,
            path: folder.join("b.wav"),
        });
        let removed = wait_until(&player, |s| s.revision != first.revision);
        assert!(folder.join("b.wav").exists(), "the file itself was deleted");
        assert_eq!(titles(&removed), vec!["a", "c"]);
        assert_eq!(removed.track.as_deref(), Some("a"), "playback was interrupted");
        assert_eq!(removed.next(), Some("c"));

        // Take the rest out too, then skip: the list has run out, so the
        // folder is read again and everything is back.
        player.send(Command::Remove {
            index: 1,
            path: folder.join("c.wav"),
        });
        player.send(Command::Remove {
            index: 0,
            path: folder.join("a.wav"),
        });
        let emptied = wait_until(&player, |s| s.queue.is_empty());
        assert_eq!(emptied.track.as_deref(), Some("a"), "the playing track plays on");
        assert_eq!(emptied.next(), None);
        player.send(Command::Next);
        let refilled = wait_until(&player, |s| s.queue.len() == 3);
        finish(player);
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(titles(&refilled), vec!["a", "b", "c"]);
        assert_eq!(refilled.state, PlaybackState::Playing);
    }

    /// A file that cannot be played leaves the list rather than being offered
    /// again, and the player moves on to the next one.
    #[test]
    fn an_unplayable_file_leaves_the_list() {
        let folder = folder_of_tracks("broken", &["a", "c"]);
        std::fs::write(folder.join("b.wav"), b"this is not a wav file").unwrap();
        let (player, finish) = running(&folder);
        wait_until(&player, |s| s.track.as_deref() == Some("a"));
        player.send(Command::Next);
        let after = wait_until(&player, |s| s.track.as_deref() == Some("c"));
        finish(player);
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(after.track.as_deref(), Some("c"));
        assert_eq!(titles(&after), vec!["a", "c"]);
    }

    /// Turning shuffle on mid-list keeps the music playing.
    #[test]
    fn a_shuffle_change_does_not_interrupt_the_track() {
        let folder = folder_of_tracks("shuffle", &["a", "b", "c", "d"]);
        let (player, finish) = running(&folder);
        let first = wait_until(&player, |s| s.track.as_deref() == Some("a"));
        player.send(Command::Reload {
            folder: folder.to_string_lossy().into_owned(),
            shuffle: true,
        });
        let after = wait_until(&player, |s| s.revision != first.revision);
        finish(player);
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(after.track.as_deref(), Some("a"));
        assert_eq!(after.playing, Some(0));
        assert_eq!(after.queue[0].title, "a");
        let mut rest: Vec<&str> = titles(&after)[1..].to_vec();
        rest.sort();
        assert_eq!(rest, vec!["b", "c", "d"]);
    }
}
