//! The live media players: one worker per Media Player source in the active
//! scene.
//!
//! Same shape and the same reasons as [`super::cue_feed`] — a worker is keyed by
//! `SourceConfig.name`, the identity key `ExternalFeeds` is routed by, and is
//! started and retired from `home::on_sources_changed`, beside
//! `sync_engine_sources`, because that is the moment the rings appear and
//! disappear. Only the active scene's players run: a media player in a scene
//! nobody is on has no ring to feed, and would be reading a folder off disk for
//! nothing.
//!
//! Playback state is session state, like monitoring. A player starts playing
//! when its scene becomes active and forgets it was paused when the scene
//! changes, which is the same rule the mixer's monitor toggles follow.
//!
//! Everything here runs on the UI thread except the worker bodies, which never
//! touch `App`: they report what they are doing through their own `Status` and
//! bump a shared generation counter, and the pump notices the counter has moved
//! and re-derives the labels. Same pattern as `tts::usage`.

use crate::audio::ExternalFeeds;
use crate::t;
use crate::config::{SourceConfig, SourceKindConfig};
use crate::media::player::{Command, Player, Status};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct MediaPlayers {
    players: RefCell<HashMap<String, Player>>,
    /// Bumped by any worker whose status changes; polled by the pump.
    generation: Arc<AtomicU64>,
}

impl MediaPlayers {
    /// How many times any player's status has changed. The pump compares this
    /// with what it last saw rather than being sent an event, because the
    /// writers are worker threads and the readers are labels.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Brings the running players in line with `sources`: starts one for every
    /// Media Player in the active scene, retires the ones whose source has gone,
    /// and tells a surviving one when its folder or shuffle setting changed.
    ///
    /// Restarting a player whose settings did not move would restart its music,
    /// and this is called on every scene edit — including edits to entirely
    /// different sources.
    pub fn apply(&self, sources: &[SourceConfig], feeds: &ExternalFeeds) {
        let wanted: Vec<(String, String, bool)> = sources
            .iter()
            .filter_map(|source| match &source.kind {
                SourceKindConfig::MediaPlayer(media) => Some((
                    source.name.clone(),
                    media.folder.clone(),
                    media.shuffle,
                )),
                _ => None,
            })
            .collect();

        let retired: Vec<Player> = {
            let mut players = self.players.borrow_mut();
            let gone: Vec<String> = players
                .keys()
                .filter(|name| !wanted.iter().any(|(wanted, ..)| wanted == *name))
                .cloned()
                .collect();
            let retired = gone.iter().filter_map(|name| players.remove(name)).collect();

            for (name, folder, shuffle) in wanted {
                match players.get_mut(&name) {
                    Some(player) => {
                        if player.folder != folder || player.shuffle != shuffle {
                            player.folder = folder.clone();
                            player.shuffle = shuffle;
                            player.send(Command::Reload { folder, shuffle });
                        }
                    }
                    None => {
                        let player = Player::start(
                            name.clone(),
                            feeds.clone(),
                            folder,
                            shuffle,
                            self.generation.clone(),
                        );
                        players.insert(name, player);
                    }
                }
            }
            retired
        };
        // Stopped outside the borrow: `stop` joins a thread, and nothing that
        // waits should be holding a `RefCell` the rest of the UI reaches for.
        for player in retired {
            player.stop();
        }
    }

    /// Every running player's state, keyed by source name, for the labels.
    pub fn statuses(&self) -> HashMap<String, Status> {
        self.players
            .borrow()
            .iter()
            .map(|(name, player)| (name.clone(), player.status()))
            .collect()
    }

    pub fn status(&self, source_name: &str) -> Option<Status> {
        self.players.borrow().get(source_name).map(Player::status)
    }

    /// Sends a transport command to one player. Nothing happens if that source
    /// is not a media player in the active scene — a keybinding names a source
    /// that may live in another scene entirely.
    pub fn send(&self, source_name: &str, command: Command) -> bool {
        match self.players.borrow().get(source_name) {
            Some(player) => {
                player.send(command);
                true
            }
            None => false,
        }
    }

    /// Retires every player. Used at shutdown.
    pub fn stop_all(&self) {
        let retired: Vec<Player> = self.players.borrow_mut().drain().map(|(_, p)| p).collect();
        for player in retired {
            player.stop();
        }
    }
}

/// What a media player strip's Upcoming tracks field says: the tracks coming
/// next, one per line and numbered, soonest first.
///
/// Numbered rather than bare so a screen reader arrowing down the field says
/// how far ahead each one is, and the first line is the next track because that
/// is what the field is opened for — what is playing *now* is already in the
/// strip's and the Sources list's labels. `None` is a player with no worker,
/// which a strip in the active scene only has for the instant before it starts.
pub fn upcoming_text(status: Option<&Status>) -> String {
    use crate::media::player::PlaybackState;
    let Some(status) = status else {
        return t!("Nothing queued");
    };
    if !status.upcoming.is_empty() {
        return status
            .upcoming
            .iter()
            .enumerate()
            .map(|(index, track)| format!("{}. {track}", index + 1))
            .collect::<Vec<_>>()
            .join("
");
    }
    match status.state {
        PlaybackState::NoFolder => t!("No folder is set"),
        PlaybackState::NoFiles => t!("The folder has no audio files Pubsplash can play"),
        PlaybackState::Playing | PlaybackState::Paused => t!("Nothing queued"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upcoming_field_numbers_the_tracks_soonest_first() {
        let status = Status {
            state: crate::media::player::PlaybackState::Playing,
            track: Some("So What".into()),
            upcoming: vec!["Blue in Green".into(), "Freddie Freeloader".into()],
            previous: None,
        };
        assert_eq!(
            upcoming_text(Some(&status)),
            "1. Blue in Green
2. Freddie Freeloader"
        );
    }

    #[test]
    fn an_empty_upcoming_field_says_why() {
        let no_folder = Status::default();
        assert_eq!(upcoming_text(Some(&no_folder)), "No folder is set");
        let no_files = Status {
            state: crate::media::player::PlaybackState::NoFiles,
            ..Status::default()
        };
        assert_eq!(
            upcoming_text(Some(&no_files)),
            "The folder has no audio files Pubsplash can play"
        );
        assert_eq!(upcoming_text(None), "Nothing queued");
    }
    use crate::config::MediaPlayerSourceConfig;

    fn media_source(name: &str, folder: &str) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            kind: SourceKindConfig::MediaPlayer(MediaPlayerSourceConfig {
                folder: folder.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_media_source_gets_a_player_and_loses_it_with_the_scene() {
        let feeds = ExternalFeeds::default();
        let players = MediaPlayers::default();

        players.apply(&[media_source("Media Player", "")], &feeds);
        assert_eq!(players.players.borrow().len(), 1);

        // A scene with no media player in it retires the worker.
        players.apply(&[], &feeds);
        assert!(players.players.borrow().is_empty());
    }

    /// The property `apply` exists for: an edit to some other source must not
    /// restart the music.
    #[test]
    fn an_unrelated_edit_leaves_the_player_alone() {
        let feeds = ExternalFeeds::default();
        let players = MediaPlayers::default();
        players.apply(&[media_source("Media Player", "")], &feeds);
        let first = players.players.borrow()["Media Player"].thread_id();

        players.apply(
            &[
                media_source("Media Player", ""),
                SourceConfig {
                    name: "Microphone".to_string(),
                    ..Default::default()
                },
            ],
            &feeds,
        );

        assert_eq!(
            players.players.borrow()["Media Player"].thread_id(),
            first,
            "the same worker is still playing"
        );
        players.stop_all();
    }

    #[test]
    fn a_folder_change_reaches_the_running_player() {
        let feeds = ExternalFeeds::default();
        let players = MediaPlayers::default();
        players.apply(&[media_source("Media Player", "")], &feeds);
        let first = players.players.borrow()["Media Player"].thread_id();

        players.apply(&[media_source("Media Player", r"C:\music")], &feeds);

        let borrowed = players.players.borrow();
        let player = &borrowed["Media Player"];
        assert_eq!(player.thread_id(), first, "reloaded, not restarted");
        assert_eq!(player.folder, r"C:\music");
        drop(borrowed);
        players.stop_all();
    }
}
