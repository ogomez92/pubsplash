//! The Media Player source: a folder of music, played into the mixer.
//!
//! Two halves. This file is the pure one — which files count as music, what
//! order they play in — and [`player`] is the worker thread that decodes them
//! and feeds [`crate::audio::ExternalFeeds`]. The UI-side lifecycle (one worker
//! per media source in the active scene) is [`crate::ui::media`].
//!
//! A media player is an ordinary mixer source in every respect that matters: it
//! has a strip, a fader, a mute, sends, and its own monitor toggle. What is
//! special about it is the ducking, and that is not here either — the engine
//! owns it, because only the engine can see what every *other* source is doing
//! in the same block (see [`crate::audio::mixer::Ducker`]).

pub mod decode;
pub mod fade;
pub mod player;
pub mod schedule;
pub mod scheduler;

use rand::seq::SliceRandom;
use std::path::{Path, PathBuf};

/// The file extensions the player will try, lowercased.
///
/// This is symphonia's coverage and nothing more: a file it cannot decode would
/// be picked, announced, and then produce silence for as long as the track
/// would have lasted, which is worse than never picking it. `.opus` is the
/// notable absence — symphonia 0.5 has no Opus decoder — as are `.wma` and
/// `.ape`, which it has never supported.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "mp4", "aac", "flac", "ogg", "oga", "wav", "wave", "aif", "aiff", "aifc",
    "caf", "mka", "mp1", "mp2",
];

/// A ceiling on how many files one folder contributes, so pointing a source at
/// a drive root cannot spend the session walking it.
const MAX_FILES: usize = 50_000;

/// A ceiling on how deep the walk goes, for the same reason.
const MAX_DEPTH: usize = 12;

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| SUPPORTED_EXTENSIONS.contains(&e.as_str()))
}

/// What the user is told a file is called: its name without the extension.
pub fn track_title(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Every playable file under `root`, including its subfolders, in a stable
/// order.
///
/// Subfolders are included because a music folder is almost always a folder of
/// album folders, and sorted because that order is what "shuffle off" means —
/// without it the order would be whatever the filesystem happened to hand back,
/// which is neither alphabetical nor stable between runs.
pub fn scan_folder(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A folder that cannot be read (permissions, a disconnected drive)
            // is skipped rather than failing the scan: the rest of the library
            // is still playable, and the user hears about an empty result
            // through the source's own label.
            Err(e) => {
                log::debug!("Media player: skipping {}: {e}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                // Symlinked directories are not followed: a link back up the
                // tree would make the walk unbounded.
                Ok(kind) if kind.is_dir() && depth < MAX_DEPTH => stack.push((path, depth + 1)),
                Ok(kind) if kind.is_file() && is_supported(&path) => files.push(path),
                _ => {}
            }
            if files.len() >= MAX_FILES {
                log::warn!(
                    "Media player: {} holds more than {MAX_FILES} playable files; the rest are ignored",
                    root.display()
                );
                files.sort();
                return files;
            }
        }
    }
    files.sort();
    files
}

/// Process-wide, so two playlists — or one playlist and the one that replaced
/// it in a new worker — can never be mistaken for each other by a revision
/// number alone. See [`Playlist::revision`].
static NEXT_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn new_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The songs a media player will play, in the order it will play them, and
/// where it is up to.
///
/// The folder is where songs come *from*: it is read when the player starts or
/// is pointed somewhere new, and again only when this list runs out. Between
/// those two moments the list is the user's — they can play any entry, remove
/// entries, or add a file of their own — and nothing re-reads the folder behind
/// their back and undoes it.
///
/// Shuffle decides the order when the list is filled, so the list shown is the
/// order that will play.
pub struct Playlist {
    entries: Vec<PathBuf>,
    /// The entry [`Playlist::next`] hands out next.
    cursor: usize,
    /// The entry playing now, if it is still in the list — removing the track
    /// that is playing leaves it playing, but no longer listed.
    playing: Option<usize>,
    shuffle: bool,
    revision: u64,
}

impl Playlist {
    /// A list of `files`, in folder order or shuffled. Nothing is playing yet.
    pub fn new(files: Vec<PathBuf>, shuffle: bool) -> Self {
        let mut playlist = Self {
            entries: Vec::new(),
            cursor: 0,
            playing: None,
            shuffle,
            revision: new_revision(),
        };
        playlist.refill(files, None);
        playlist
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every entry, in playing order.
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// Which entry is playing, when it is still in the list.
    pub fn playing(&self) -> Option<usize> {
        self.playing
    }

    /// Changes whenever the *entries* change — not when the playing marker
    /// moves — so a list on screen can tell a track starting from a list that
    /// needs rebuilding. Unique across every playlist in the process.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether every entry has been handed out, which is when the folder is
    /// read again.
    pub fn is_exhausted(&self) -> bool {
        self.cursor >= self.entries.len()
    }

    /// Replaces the list with a fresh pass over `files`. `last_played` is the
    /// track that just ended: a fresh shuffle is allowed to start with it,
    /// which is the one repeat a listener always notices, so it is moved out
    /// of the way.
    pub fn refill(&mut self, files: Vec<PathBuf>, last_played: Option<&Path>) {
        let mut entries = files;
        if self.shuffle {
            entries.shuffle(&mut rand::thread_rng());
            if entries.len() > 1 && last_played.is_some_and(|last| entries[0] == last) {
                entries.swap(0, 1);
            }
        }
        self.entries = entries;
        self.cursor = 0;
        self.playing = None;
        self.revision = new_revision();
    }

    /// The next entry to play, or `None` once the list has run out.
    pub fn next(&mut self) -> Option<PathBuf> {
        let picked = self.entries.get(self.cursor)?.clone();
        self.playing = Some(self.cursor);
        self.cursor += 1;
        Some(picked)
    }

    /// The entry [`Playlist::next`] would return, without taking it. `None` at
    /// the end of the list, where what comes next depends on a folder that has
    /// not been read yet.
    pub fn peek(&self) -> Option<&PathBuf> {
        self.entries.get(self.cursor)
    }

    /// Where "previous track" goes: the entry above the one playing. From the
    /// first entry that is the entry itself, which restarts it — what a media
    /// key does on a player's first track, and better than a key that does
    /// nothing. With the playing track removed, it is the entry above the gap.
    fn previous_index(&self) -> Option<usize> {
        let index = match self.playing {
            Some(playing) => playing.saturating_sub(1),
            None => self.cursor.checked_sub(1)?,
        };
        (index < self.entries.len()).then_some(index)
    }

    /// Steps back one entry and returns it, to be played from the start.
    pub fn previous(&mut self) -> Option<PathBuf> {
        let index = self.previous_index()?;
        self.play_at(index)
    }

    /// The entry [`Playlist::previous`] would play, without moving.
    pub fn peek_previous(&self) -> Option<&PathBuf> {
        self.entries.get(self.previous_index()?)
    }

    /// Makes `index` the playing entry, so the list carries on from the one
    /// after it, and returns it.
    pub fn play_at(&mut self, index: usize) -> Option<PathBuf> {
        let picked = self.entries.get(index)?.clone();
        self.playing = Some(index);
        self.cursor = index + 1;
        Some(picked)
    }

    /// Puts a file of the user's choosing into the list just below the track
    /// playing now, and makes it the playing entry. The file does not have to
    /// be in the folder; it is listed for the rest of this pass only.
    pub fn insert_and_play(&mut self, path: PathBuf) {
        let at = self
            .playing
            .map_or(self.cursor, |playing| playing + 1)
            .min(self.entries.len());
        self.entries.insert(at, path);
        self.playing = Some(at);
        self.cursor = at + 1;
        self.revision = new_revision();
    }

    /// Takes one entry out of the list. The file on disk is untouched. Taking
    /// out the entry that is playing leaves it playing; the list carries on
    /// from the entry that followed it.
    pub fn remove(&mut self, index: usize) -> Option<PathBuf> {
        if index >= self.entries.len() {
            return None;
        }
        let removed = self.entries.remove(index);
        if index < self.cursor {
            self.cursor -= 1;
        }
        self.playing = match self.playing {
            Some(playing) if playing == index => None,
            Some(playing) if playing > index => Some(playing - 1),
            other => other,
        };
        self.revision = new_revision();
        Some(removed)
    }

    /// Where `path` is in the list, trusting `index` when it still holds it.
    /// The user picks an entry by its row, and the list may have moved under
    /// them by the time the worker sees it; the path is what they meant.
    pub fn find(&self, index: usize, path: &Path) -> Option<usize> {
        if self.entries.get(index).is_some_and(|entry| entry == path) {
            return Some(index);
        }
        self.entries.iter().position(|entry| entry == path)
    }

    /// Turns shuffle on or off for what has not played yet. What has already
    /// played, and the track playing now, stay where they are.
    pub fn set_shuffle(&mut self, shuffle: bool) {
        self.shuffle = shuffle;
        let from = self.cursor.min(self.entries.len());
        let rest = &mut self.entries[from..];
        if shuffle {
            rest.shuffle(&mut rand::thread_rng());
        } else {
            // Folder order: `scan_folder` sorts, so sorting puts it back.
            rest.sort();
        }
        self.revision = new_revision();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zero-padded so the folder's sorted order is the numeric one.
    fn files(count: usize) -> Vec<PathBuf> {
        (0..count).map(track).collect()
    }

    #[test]
    fn only_decodable_extensions_are_picked_up() {
        assert!(is_supported(Path::new("song.mp3")));
        assert!(is_supported(Path::new("song.FLAC")), "case-insensitive");
        assert!(!is_supported(Path::new("cover.jpg")));
        assert!(!is_supported(Path::new("notes.txt")));
        // No decoder for these, so they must never be queued.
        assert!(!is_supported(Path::new("song.opus")));
        assert!(!is_supported(Path::new("song.wma")));
        assert!(!is_supported(Path::new("noextension")));
    }

    /// Built from components rather than a literal, because a separator is not
    /// a separator on both platforms: `C:\music\01 Hello.mp3` is one long file
    /// name to a Unix `Path`, and the test would be asserting nothing.
    #[test]
    fn a_track_is_named_by_its_filename_without_the_extension() {
        let path = Path::new("music").join("01 Hello.mp3");
        assert_eq!(track_title(&path), "01 Hello");
    }

    fn track(n: usize) -> PathBuf {
        PathBuf::from(format!("track{n:02}.mp3"))
    }

    /// Without shuffle the list is the folder's own order, and it runs out
    /// rather than wrapping: the end of the list is when the folder is read
    /// again, which is the worker's job.
    #[test]
    fn in_order_playback_plays_the_list_then_runs_out() {
        let mut playlist = Playlist::new(files(3), false);
        let played: Vec<PathBuf> = (0..5).filter_map(|_| playlist.next()).collect();
        assert_eq!(played, files(3));
        assert!(playlist.is_exhausted());
    }

    /// A shuffle that repeats a track before playing the others is the thing
    /// users notice and complain about, so a pass plays each file exactly once.
    #[test]
    fn a_shuffled_pass_plays_every_file_once() {
        let mut playlist = Playlist::new(files(20), true);
        let mut played: Vec<PathBuf> = (0..20).filter_map(|_| playlist.next()).collect();
        played.sort();
        assert_eq!(played, files(20));
    }

    /// The list shown is the order that plays, shuffled or not.
    #[test]
    fn the_list_is_the_order_that_plays() {
        for shuffle in [false, true] {
            let mut playlist = Playlist::new(files(10), shuffle);
            let shown = playlist.entries().to_vec();
            let played: Vec<PathBuf> = (0..10).filter_map(|_| playlist.next()).collect();
            assert_eq!(shown, played, "shuffle: {shuffle}");
        }
    }

    /// The one repeat a refill can produce: the last track of a pass coming
    /// back as the first of the next.
    #[test]
    fn a_refill_does_not_repeat_the_track_that_just_played() {
        for _ in 0..200 {
            let mut playlist = Playlist::new(files(4), true);
            let mut last = None;
            while let Some(path) = playlist.next() {
                last = Some(path);
            }
            let last = last.unwrap();
            playlist.refill(files(4), Some(&last));
            assert_ne!(playlist.entries()[0], last, "repeated across the refill");
        }
    }

    /// What "skip to the next track" announces has to be what then plays.
    #[test]
    fn peek_is_what_next_hands_back() {
        for shuffle in [false, true] {
            let mut playlist = Playlist::new(files(4), shuffle);
            for _ in 0..4 {
                let peeked = playlist.peek().cloned();
                assert!(peeked.is_some());
                assert_eq!(peeked, playlist.next(), "shuffle: {shuffle}");
            }
            assert_eq!(playlist.peek(), None, "nothing is known past the end");
        }
    }

    /// Playing an entry from the list carries on from the one after it.
    #[test]
    fn playing_an_entry_carries_on_from_there() {
        let mut playlist = Playlist::new(files(5), false);
        playlist.next();
        assert_eq!(playlist.play_at(3), Some(track(3)));
        assert_eq!(playlist.playing(), Some(3));
        assert_eq!(playlist.next(), Some(track(4)));
        assert!(playlist.is_exhausted());
        // And back up the list, too.
        assert_eq!(playlist.play_at(0), Some(track(0)));
        assert_eq!(playlist.next(), Some(track(1)));
        assert_eq!(playlist.play_at(99), None);
    }

    /// Back goes to the entry above, and from the top restarts it.
    #[test]
    fn previous_is_the_entry_above() {
        let mut playlist = Playlist::new(files(4), false);
        assert_eq!(playlist.peek_previous(), None, "nothing played yet");
        assert_eq!(playlist.previous(), None);
        for _ in 0..3 {
            playlist.next();
        }
        assert_eq!(playlist.peek_previous(), Some(&track(1)));
        assert_eq!(playlist.previous(), Some(track(1)));
        assert_eq!(playlist.previous(), Some(track(0)));
        assert_eq!(playlist.peek_previous(), Some(&track(0)), "the top restarts");
        assert_eq!(playlist.previous(), Some(track(0)));
        assert_eq!(playlist.next(), Some(track(1)));
    }

    /// Removing entries never changes what plays next unless it is what was
    /// removed.
    #[test]
    fn removing_entries_keeps_the_place() {
        let mut playlist = Playlist::new(files(5), false);
        playlist.next();
        playlist.next(); // playing track 1
        let before = playlist.revision();
        assert_eq!(playlist.remove(0), Some(track(0)));
        assert_ne!(playlist.revision(), before);
        assert_eq!(playlist.playing(), Some(0), "track 1 moved up a row");
        assert_eq!(playlist.peek(), Some(&track(2)));
        // An upcoming entry: the one after it plays instead.
        assert_eq!(playlist.remove(1), Some(track(2)));
        assert_eq!(playlist.peek(), Some(&track(3)));
        assert_eq!(playlist.remove(99), None);
    }

    /// The playing entry can be removed: it stops being listed, and the list
    /// carries on from the entry that followed it.
    #[test]
    fn removing_the_playing_entry_carries_on_below_it() {
        let mut playlist = Playlist::new(files(4), false);
        playlist.next();
        playlist.next(); // playing track 1
        playlist.remove(1);
        assert_eq!(playlist.playing(), None);
        assert_eq!(playlist.peek(), Some(&track(2)));
        assert_eq!(playlist.peek_previous(), Some(&track(0)));
        assert_eq!(playlist.entries(), &[track(0), track(2), track(3)]);
    }

    #[test]
    fn removing_everything_runs_the_list_out() {
        let mut playlist = Playlist::new(files(2), false);
        playlist.next();
        playlist.remove(0);
        playlist.remove(0);
        assert!(playlist.is_empty());
        assert!(playlist.is_exhausted());
        assert_eq!(playlist.next(), None);
        assert_eq!(playlist.peek_previous(), None);
    }

    /// An opened file goes in just below what it interrupted and plays; the
    /// list then carries on where it was.
    #[test]
    fn an_opened_file_joins_the_list_where_it_plays() {
        let mut playlist = Playlist::new(files(3), false);
        playlist.next(); // playing track 0
        let opened = PathBuf::from("elsewhere.mp3");
        playlist.insert_and_play(opened.clone());
        assert_eq!(
            playlist.entries(),
            &[track(0), opened.clone(), track(1), track(2)]
        );
        assert_eq!(playlist.playing(), Some(1));
        assert_eq!(playlist.peek_previous(), Some(&track(0)));
        assert_eq!(playlist.next(), Some(track(1)));
        // Before anything has played, it goes on top.
        let mut fresh = Playlist::new(files(2), false);
        fresh.insert_and_play(opened.clone());
        assert_eq!(fresh.entries()[0], opened);
        assert_eq!(fresh.next(), Some(track(0)));
    }

    /// The user's row and the worker's list can disagree for a moment; the
    /// path is what they meant.
    #[test]
    fn find_trusts_the_index_then_the_path() {
        let playlist = Playlist::new(files(4), false);
        assert_eq!(playlist.find(2, &track(2)), Some(2));
        assert_eq!(playlist.find(0, &track(3)), Some(3));
        assert_eq!(playlist.find(0, Path::new("gone.mp3")), None);
    }

    /// Turning shuffle on mid-list leaves what has played, and what is
    /// playing, alone.
    #[test]
    fn shuffle_reorders_only_what_is_still_to_come() {
        let mut playlist = Playlist::new(files(30), false);
        playlist.next();
        playlist.next();
        playlist.set_shuffle(true);
        assert_eq!(&playlist.entries()[..2], &[track(0), track(1)]);
        assert_eq!(playlist.playing(), Some(1));
        let mut rest = playlist.entries()[2..].to_vec();
        rest.sort();
        assert_eq!(rest, files(30)[2..].to_vec());
        playlist.set_shuffle(false);
        assert_eq!(playlist.entries(), files(30).as_slice(), "back in folder order");
    }

    #[test]
    fn an_empty_folder_plays_nothing() {
        let mut playlist = Playlist::new(Vec::new(), true);
        assert!(playlist.is_empty());
        assert!(playlist.is_exhausted());
        assert_eq!(playlist.peek(), None);
        assert_eq!(playlist.next(), None);
    }

    #[test]
    fn a_scan_finds_music_in_subfolders_and_ignores_everything_else() {
        let root = std::env::temp_dir().join(format!("pubsplash-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("album")).unwrap();
        std::fs::write(root.join("b.mp3"), b"").unwrap();
        std::fs::write(root.join("cover.jpg"), b"").unwrap();
        std::fs::write(root.join("album").join("a.flac"), b"").unwrap();

        let found = scan_folder(&root);

        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0], root.join("album").join("a.flac"), "sorted");
        assert_eq!(found[1], root.join("b.mp3"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_that_is_not_there_scans_to_nothing() {
        assert!(scan_folder(Path::new(r"Z:\definitely\not\here")).is_empty());
    }
}
