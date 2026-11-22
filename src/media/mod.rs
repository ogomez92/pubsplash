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

/// The order a folder's files play in, and where the player is up to.
///
/// Exhausting the list is not the end of anything: [`Playlist::next`] wraps,
/// reshuffling as it goes, so a source left alone plays forever.
pub struct Playlist {
    files: Vec<PathBuf>,
    /// Indices into `files`, in playing order.
    order: Vec<usize>,
    /// The pass after this one, already decided. Shuffling it only at the wrap
    /// would leave every track past the end of this pass unknown, and
    /// [`Playlist::upcoming`] is asked for several tracks ahead — the whole
    /// point of which is presenting a song on air before it plays, so a list
    /// that turned out wrong at the boundary would be worse than none.
    following: Vec<usize>,
    /// How far into `order` the player has got.
    position: usize,
    shuffle: bool,
    /// What has been played, oldest first, ending with the track playing now.
    /// Kept as paths rather than positions in `order` because a pass is thrown
    /// away when it ends, and "previous track" across the wrap has to reach the
    /// end of the pass before it.
    history: Vec<PathBuf>,
    /// Tracks stepped back over with [`Playlist::previous`], the soonest last.
    /// [`Playlist::next`] takes from here before the pass, so going back and
    /// then forward again lands where the user started rather than skipping
    /// on through the pass.
    ahead: Vec<PathBuf>,
}

/// How far back "previous track" can reach.
const HISTORY: usize = 100;

impl Playlist {
    pub fn new(files: Vec<PathBuf>, shuffle: bool) -> Self {
        let mut playlist = Self {
            files,
            order: Vec::new(),
            following: Vec::new(),
            position: 0,
            shuffle,
            history: Vec::new(),
            ahead: Vec::new(),
        };
        playlist.order = playlist.pass(None);
        playlist.following = playlist.pass(playlist.order.last().copied());
        playlist
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// The files this playlist was built from, in folder order. Compared
    /// against a fresh scan so an unchanged folder does not reshuffle a pass
    /// that is already in progress.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The next file to play, wrapping (and reshuffling) at the end.
    pub fn next(&mut self) -> Option<PathBuf> {
        if let Some(picked) = self.ahead.pop() {
            self.remember(picked.clone());
            return Some(picked);
        }
        let picked = self.peek()?;
        self.remember(picked.clone());
        self.position += 1;
        // Wrapped eagerly, the moment the pass runs out, rather than on the way
        // into the next `next()`. That is what makes [`Playlist::peek`] able to
        // answer at all: deferring the wrap leaves the first track of the next
        // pass out of reach, and "skip to the next track" has to be able to say
        // what it is about to play before it plays it.
        if self.position >= self.order.len() {
            self.order = std::mem::take(&mut self.following);
            self.following = self.pass(self.order.last().copied());
            self.position = 0;
        }
        Some(picked)
    }

    /// The file [`Playlist::next`] would return, without taking it.
    ///
    /// This is what the announcement on a skip is built from, so it has to be
    /// the truth rather than a guess — see the eager wrap in `next`.
    pub fn peek(&self) -> Option<PathBuf> {
        self.upcoming(1).into_iter().next()
    }

    /// The next `count` files [`Playlist::next`] will hand back, in order —
    /// across the end of the pass too, since the pass after it is already
    /// shuffled.
    ///
    /// Never longer than the folder: a two-file folder has two tracks coming,
    /// not a list of four that names each of them twice.
    pub fn upcoming(&self, count: usize) -> Vec<PathBuf> {
        let from_pass = self
            .order
            .get(self.position..)
            .unwrap_or_default()
            .iter()
            .chain(&self.following)
            .filter_map(|&index| self.files.get(index).cloned());
        self.ahead
            .iter()
            .rev()
            .cloned()
            .chain(from_pass)
            .take(count.min(self.files.len().max(self.ahead.len())))
            .collect()
    }

    /// Steps back one track and returns it, to be played from the start.
    ///
    /// The track playing now goes back on the front of what is coming, so a
    /// skip afterwards returns to it. With nothing earlier to go back to — the
    /// first track of a session — the track playing now is returned instead,
    /// which restarts it: the same thing a media key does on a player's first
    /// track, and better than a key that does nothing.
    pub fn previous(&mut self) -> Option<PathBuf> {
        if self.history.len() >= 2
            && let Some(current) = self.history.pop()
        {
            self.ahead.push(current);
        }
        self.history.last().cloned()
    }

    /// The file [`Playlist::previous`] would return, without moving. What the
    /// announcement on "previous track" is built from.
    pub fn peek_previous(&self) -> Option<&PathBuf> {
        let len = self.history.len();
        self.history.get(len.saturating_sub(2))
    }

    /// Records a file played from outside the playlist — Open file — so that
    /// "previous track" from the track after it comes back to it, and from it
    /// goes back to what it interrupted.
    ///
    /// Anything stepped back over is forgotten: the user went somewhere new,
    /// and "next" now means the pass again, as it does after a browser's back
    /// button and a fresh link.
    pub fn played(&mut self, path: PathBuf) {
        self.ahead.clear();
        self.remember(path);
    }

    /// Carries what has been played across a rescan that changed the folder's
    /// contents, so a file arriving mid-session does not cost the user the way
    /// back.
    pub fn inherit_history(&mut self, from: Playlist) {
        self.history = from.history;
    }

    fn remember(&mut self, path: PathBuf) {
        self.history.push(path);
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
    }

    /// One pass's play order. `avoid_first` is the last track of the pass
    /// before it: a fresh shuffle is allowed to start with it, which is the one
    /// repeat a listener always notices, so it is moved out of the way.
    fn pass(&self, avoid_first: Option<usize>) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.files.len()).collect();
        if !self.shuffle {
            return order;
        }
        order.shuffle(&mut rand::thread_rng());
        if order.len() > 1 && order.first().copied() == avoid_first {
            order.swap(0, 1);
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|n| PathBuf::from(format!("track{n}.mp3")))
            .collect()
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

    /// Without shuffle the list is the folder's own order, over and over.
    #[test]
    fn in_order_playback_wraps_round() {
        let mut playlist = Playlist::new(files(3), false);
        let played: Vec<PathBuf> = (0..7).filter_map(|_| playlist.next()).collect();
        assert_eq!(
            played,
            vec![
                PathBuf::from("track0.mp3"),
                PathBuf::from("track1.mp3"),
                PathBuf::from("track2.mp3"),
                PathBuf::from("track0.mp3"),
                PathBuf::from("track1.mp3"),
                PathBuf::from("track2.mp3"),
                PathBuf::from("track0.mp3"),
            ]
        );
    }

    /// A shuffle that repeats a track before playing the others is the thing
    /// users notice and complain about, so a pass plays each file exactly once.
    #[test]
    fn a_shuffled_pass_plays_every_file_once() {
        let mut playlist = Playlist::new(files(20), true);
        let mut played: Vec<PathBuf> = (0..20).filter_map(|_| playlist.next()).collect();
        played.sort();
        let mut expected = files(20);
        expected.sort();
        assert_eq!(played, expected);
    }

    /// The one repeat a wrap can produce: the last track of a pass coming back
    /// as the first of the next.
    #[test]
    fn a_reshuffle_does_not_repeat_the_track_that_just_played() {
        for _ in 0..200 {
            let mut playlist = Playlist::new(files(4), true);
            let played: Vec<PathBuf> = (0..8).filter_map(|_| playlist.next()).collect();
            assert_ne!(played[3], played[4], "repeated across the wrap");
        }
    }

    /// What "skip to the next track" announces has to be what then plays,
    /// including across the wrap where a reshuffle happens.
    #[test]
    fn peek_is_what_next_hands_back() {
        for shuffle in [false, true] {
            let mut playlist = Playlist::new(files(4), shuffle);
            for _ in 0..12 {
                let peeked = playlist.peek();
                assert_eq!(peeked, playlist.next(), "shuffle: {shuffle}");
                assert!(peeked.is_some());
            }
        }
    }

    /// The list shown ahead of time has to be what then plays, track for
    /// track — across the wrap above all, where a shuffled pass used to be
    /// decided only once the previous one ran out.
    #[test]
    fn upcoming_is_what_next_hands_back_in_turn() {
        for shuffle in [false, true] {
            for _ in 0..50 {
                let mut playlist = Playlist::new(files(5), shuffle);
                for _ in 0..13 {
                    let ahead = playlist.upcoming(4);
                    assert_eq!(ahead.len(), 4);
                    let mut probe = Playlist {
                        files: playlist.files.clone(),
                        order: playlist.order.clone(),
                        following: playlist.following.clone(),
                        position: playlist.position,
                        shuffle,
                        history: playlist.history.clone(),
                        ahead: playlist.ahead.clone(),
                    };
                    let played: Vec<PathBuf> = (0..4).filter_map(|_| probe.next()).collect();
                    assert_eq!(ahead, played, "shuffle: {shuffle}");
                    playlist.next();
                }
            }
        }
    }

    #[test]
    fn upcoming_never_names_more_tracks_than_the_folder_holds() {
        assert_eq!(Playlist::new(files(2), true).upcoming(4).len(), 2);
        assert_eq!(
            Playlist::new(files(1), true).upcoming(4),
            vec![PathBuf::from("track0.mp3")]
        );
        assert!(Playlist::new(Vec::new(), true).upcoming(4).is_empty());
    }

    /// Back goes to what played before, forward again returns to where the
    /// user started, and only then does the pass carry on.
    #[test]
    fn previous_steps_back_and_next_retraces_the_steps() {
        let mut playlist = Playlist::new(files(4), false);
        let track = |n: usize| PathBuf::from(format!("track{n}.mp3"));
        for _ in 0..3 {
            playlist.next();
        }
        // Playing track2.
        assert_eq!(playlist.peek_previous(), Some(&track(1)));
        assert_eq!(playlist.previous(), Some(track(1)));
        assert_eq!(playlist.previous(), Some(track(0)));
        // The first track has nothing before it: previous restarts it.
        assert_eq!(playlist.peek_previous(), Some(&track(0)));
        assert_eq!(playlist.previous(), Some(track(0)));
        // What is coming is the way back, then the pass where it was.
        assert_eq!(playlist.upcoming(4), vec![track(1), track(2), track(3), track(0)]);
        for n in [1, 2, 3, 0] {
            assert_eq!(playlist.peek(), Some(track(n)));
            assert_eq!(playlist.next(), Some(track(n)));
        }
    }

    /// Previous crosses the wrap, where a shuffled pass has been thrown away.
    #[test]
    fn previous_reaches_back_across_the_wrap() {
        for _ in 0..50 {
            let mut playlist = Playlist::new(files(3), true);
            let played: Vec<PathBuf> = (0..4).filter_map(|_| playlist.next()).collect();
            assert_eq!(playlist.previous().as_ref(), Some(&played[2]));
            assert_eq!(playlist.previous().as_ref(), Some(&played[1]));
            assert_eq!(playlist.next().as_ref(), Some(&played[2]));
            assert_eq!(playlist.next().as_ref(), Some(&played[3]));
        }
    }

    /// An opened file is a track like any other to previous, and forgets the
    /// way forward the user had stepped back over.
    #[test]
    fn an_opened_file_joins_the_history() {
        let mut playlist = Playlist::new(files(3), false);
        playlist.next();
        playlist.next();
        playlist.previous();
        let opened = PathBuf::from("elsewhere.mp3");
        playlist.played(opened.clone());
        assert_eq!(playlist.peek_previous(), Some(&PathBuf::from("track0.mp3")));
        assert_eq!(playlist.next(), Some(PathBuf::from("track2.mp3")));
        assert_eq!(playlist.previous(), Some(opened));
    }

    #[test]
    fn nothing_played_has_no_previous() {
        let mut playlist = Playlist::new(files(3), false);
        assert_eq!(playlist.peek_previous(), None);
        assert_eq!(playlist.previous(), None);
        assert_eq!(playlist.peek(), Some(PathBuf::from("track0.mp3")));
    }

    #[test]
    fn an_empty_playlist_has_nothing_to_peek_at() {
        assert_eq!(Playlist::new(Vec::new(), true).peek(), None);
    }

    #[test]
    fn an_empty_folder_plays_nothing_rather_than_looping() {
        let mut playlist = Playlist::new(Vec::new(), true);
        assert!(playlist.is_empty());
        assert_eq!(playlist.next(), None);
    }

    #[test]
    fn one_file_repeats_rather_than_stopping() {
        let mut playlist = Playlist::new(files(1), true);
        assert_eq!(playlist.next(), Some(PathBuf::from("track0.mp3")));
        assert_eq!(playlist.next(), Some(PathBuf::from("track0.mp3")));
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
