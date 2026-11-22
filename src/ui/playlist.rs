//! A media player strip's playlist: every song the player will play, in the
//! order it will play them, as a list the user can browse and edit.
//!
//! It replaced a read-only text field of the next four tracks. A list gives the
//! user what a screen reader already knows how to do with one — arrows, Home
//! and End, type-ahead by first letter — and three things the field could not:
//! **Enter** plays the selected song, **Delete** takes it out of the list (the
//! file is untouched, so there is no confirmation), and the context menu
//! (SHIFT+F10 or the applications key) offers both plus the player's own
//! controls. **ALT+L** brings focus here from anywhere in the main window.
//!
//! The list is the player's ([`crate::media::Playlist`]); this file only shows
//! it. Every edit is a command to the worker, and the list is redrawn from the
//! status the worker publishes back, so what is on screen is always what will
//! play — never a guess the UI made on the worker's behalf.
//!
//! ## Staying quiet
//!
//! The rules are `home::refresh_overview`'s, for the same reasons. A track
//! starting moves the "(playing)" marker, which is two `set_string`s, and
//! `set_string` on the **selected** row is announced whether or not the list
//! has focus — so a refresh never writes the selected row, and it is brought up
//! to date at the two moments the user is about to hear it anyway: selection
//! leaving it, and focus arriving. A rebuild — the entries themselves changed —
//! happens only after something the user did (Delete, Open file) or when the
//! list runs out and the folder is read again, and puts the selection back on
//! the same song.

use crate::media::player::Command;
use crate::t;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wxdragon::prelude::*;

use super::{App, WXK_DELETE};

/// Enter on a `ListBox`. wx reports the main and keypad keys differently.
const WXK_RETURN: i32 = 13;
const WXK_NUMPAD_ENTER: i32 = 370;

/// Context-menu command ids. The menu is popped up on the list, so its
/// commands come back to that list's own handlers; one set of ids serves every
/// strip.
const ID_PLAYLIST_PLAY: i32 = 2210;
const ID_PLAYLIST_REMOVE: i32 = 2211;
const ID_PLAYLIST_PAUSE: i32 = 2212;
const ID_PLAYLIST_OPEN: i32 = 2213;
const ID_PLAYLIST_FOLDER: i32 = 2214;

/// Enter can arrive twice for one press — as a key and as wx's double-click
/// translation of it — and a song started twice is a song that stutters on air.
const REPEAT_GUARD: Duration = Duration::from_millis(300);

/// One strip's playlist.
#[derive(Clone)]
pub struct PlaylistField {
    /// The source's identity name, which is what its player is filed under.
    source: String,
    pub list: ListBox,
    /// The visible name in front of the list, which is also what a screen
    /// reader names the list by on Windows (see `native_acc`).
    label: StaticText,
    shown: Rc<RefCell<Shown>>,
    /// Set while a refresh is writing to the list, so an event the write
    /// raises cannot start a second one over the top of it.
    refreshing: Rc<Cell<bool>>,
    has_focus: Rc<Cell<bool>>,
    last_play: Rc<Cell<Option<Instant>>>,
}

/// What is really in the widget, so a refresh can tell an unchanged list
/// without reading thousands of rows back over FFI.
#[derive(Default)]
struct Shown {
    /// The player's revision these rows were built from. `None` before the
    /// first fill.
    revision: Option<u64>,
    /// The placeholder row's text while the list is empty.
    placeholder: Option<String>,
    rows: Vec<String>,
    paths: Vec<PathBuf>,
    /// After Delete: the row to select once the list has been rebuilt without
    /// the entry, which is how the user hears what took its place.
    select_after: Option<usize>,
}

/// Builds a strip's playlist into `sizer`. `name` is the strip's name as the
/// user knows it.
pub fn build(
    app: &Rc<App>,
    parent: &Panel,
    sizer: &BoxSizer,
    source: &str,
    name: &str,
) -> PlaylistField {
    let row = BoxSizer::builder(Orientation::Vertical).build();
    let label = StaticText::builder(parent)
        .with_label(&label_text(name))
        .build();
    let list = ListBox::builder(parent)
        .with_size(Size::new(-1, 120))
        .build();
    super::native_acc::install(&list, &label_text(name));
    super::help::tag(&list, "tab.home.mixer.strip.playlist", "Mixer strip playlist");
    row.add(&label, 0, SizerFlag::All, 4);
    row.add(&list, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&row, 0, SizerFlag::Expand, 0);

    let field = PlaylistField {
        source: source.to_string(),
        list,
        label,
        shown: Rc::default(),
        refreshing: Rc::default(),
        has_focus: Rc::default(),
        last_play: Rc::default(),
    };
    refresh(app, &field, Write::All);

    {
        let app = app.clone();
        let field = field.clone();
        list.on_selection_changed(move |_| refresh(&app, &field, Write::Unselected));
    }
    {
        let app = app.clone();
        let field = field.clone();
        list.on_set_focus(move |event| {
            field.has_focus.set(true);
            // Whatever row focus lands on is about to be read out, so this is
            // the one refresh allowed to write it.
            refresh(&app, &field, Write::All);
            event.skip(true);
        });
    }
    {
        let field = field.clone();
        list.on_kill_focus(move |event| {
            field.has_focus.set(false);
            event.skip(true);
        });
    }
    {
        let app = app.clone();
        let field = field.clone();
        list.on_item_double_clicked(move |_| play_selected(&app, &field));
    }
    {
        let app = app.clone();
        let field = field.clone();
        list.on_key_down(move |event| match super::key_of(&event) {
            Some((WXK_RETURN | WXK_NUMPAD_ENTER, false)) => {
                event.skip(false);
                play_selected(&app, &field);
            }
            Some((WXK_DELETE, false)) => {
                event.skip(false);
                remove_selected(&app, &field);
            }
            _ => event.skip(true),
        });
    }
    bind_menu(app, &field);
    field
}

fn label_text(name: &str) -> String {
    t!("{name} playlist", name = name)
}

impl PlaylistField {
    /// Follows the strip's name when it changes in place.
    pub fn rename(&self, name: &str) {
        self.label.set_label(&label_text(name));
    }
}

/// Which rows a refresh may write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    /// Every row but the selected one: nothing the user did asked for this.
    Unselected,
    /// Every row: the selected one is about to be read out anyway.
    All,
}

/// Brings every strip's playlist up to date. Run off the pump when a player
/// reports a change.
pub fn refresh_all(app: &Rc<App>) {
    let fields: Vec<PlaylistField> = fields(app);
    for field in &fields {
        refresh(app, field, Write::Unselected);
    }
}

fn fields(app: &Rc<App>) -> Vec<PlaylistField> {
    app.widgets(|w| {
        w.mixer_strips
            .borrow()
            .iter()
            .filter_map(|strip| strip.playlist.clone())
            .collect()
    })
    .unwrap_or_default()
}

fn refresh(app: &Rc<App>, field: &PlaylistField, write: Write) {
    if field.refreshing.replace(true) {
        return;
    }
    let status = app.media.status(&field.source);
    let rows = super::media::playlist_rows(status.as_ref());
    let placeholder = rows
        .is_empty()
        .then(|| super::media::playlist_placeholder(status.as_ref()));
    let revision = status.as_ref().map(|status| status.revision);
    // Taken out rather than borrowed, so nothing is borrowed while wx runs.
    let mut shown = std::mem::take(&mut *field.shown.borrow_mut());
    let list = field.list;
    let selected = list.get_selection();

    if shown.revision.is_some()
        && shown.revision == revision
        && shown.placeholder == placeholder
        && shown.rows.len() == rows.len()
    {
        // The same entries: at most the playing marker moved.
        for (index, row) in rows.into_iter().enumerate() {
            if shown.rows[index] == row {
                continue;
            }
            if write == Write::Unselected && selected == Some(index as u32) {
                // Left stale in the cache too, so it is retried the moment the
                // row stops being the selected one.
                continue;
            }
            list.set_string(index as u32, &row);
            shown.rows[index] = row;
        }
    } else {
        let paths: Vec<PathBuf> = status
            .as_ref()
            .map(|status| status.queue.iter().map(|entry| entry.path.clone()).collect())
            .unwrap_or_default();
        // Where the selection should land once the list is refilled: the row a
        // Delete asked for, or else the same song the user was on.
        let was_on = selected
            .map(|index| index as usize)
            .and_then(|index| shown.paths.get(index).map(|path| (index, path.clone())));
        let deleted = shown.select_after.take();
        let target = match (deleted, was_on) {
            (Some(index), _) => (!paths.is_empty()).then(|| index.min(paths.len() - 1)),
            (None, Some((index, path))) => nearest(&paths, &path, index)
                .or_else(|| (!paths.is_empty()).then(|| index.min(paths.len() - 1))),
            (None, None) => None,
        };
        super::list::fill(&list, &rows, placeholder.as_deref().unwrap_or_default());
        if let Some(index) = target {
            list.set_selection(index as u32, true);
            list.ensure_visible(index as i32);
        } else if deleted.is_some() && field.has_focus.get() {
            // The last entry went: the placeholder row is not selected, so
            // nothing would say the list is now empty.
            if let Some(text) = &placeholder {
                super::help::announce(text);
            }
        }
        shown = Shown {
            revision,
            placeholder,
            rows,
            paths,
            select_after: None,
        };
    }
    *field.shown.borrow_mut() = shown;
    field.refreshing.set(false);
}

/// The occurrence of `path` nearest `index` — a file opened with Open file can
/// be listed twice, and the user was on one of them.
fn nearest(paths: &[PathBuf], path: &PathBuf, index: usize) -> Option<usize> {
    paths
        .iter()
        .enumerate()
        .filter(|(_, candidate)| *candidate == path)
        .map(|(at, _)| at)
        .min_by_key(|at| at.abs_diff(index))
}

/// The selected entry, as the row number and path the worker is sent. `None`
/// on the placeholder row.
fn selected_entry(field: &PlaylistField) -> Option<(usize, PathBuf)> {
    let shown = field.shown.borrow();
    let index = super::list::selection(&field.list, shown.paths.len())?;
    Some((index, shown.paths[index].clone()))
}

fn play_selected(app: &Rc<App>, field: &PlaylistField) {
    if field
        .last_play
        .get()
        .is_some_and(|at| at.elapsed() < REPEAT_GUARD)
    {
        return;
    }
    let Some((index, path)) = selected_entry(field) else {
        return;
    };
    field.last_play.set(Some(Instant::now()));
    super::media_transport(app, &field.source, Command::PlayAt { index, path });
}

/// Takes the selected entry out of the list. No confirmation: the file stays
/// where it is, and the folder brings it back when the list runs out.
fn remove_selected(app: &Rc<App>, field: &PlaylistField) {
    let Some((index, path)) = selected_entry(field) else {
        return;
    };
    field.shown.borrow_mut().select_after = Some(index);
    app.media.send(&field.source, Command::Remove { index, path });
}

fn bind_menu(app: &Rc<App>, field: &PlaylistField) {
    let list = field.list;
    {
        let app = app.clone();
        let field = field.clone();
        list.bind_internal(EventType::CONTEXT_MENU, move |_| {
            let on_entry = selected_entry(&field).is_some();
            let paused = app.media.status(&field.source).is_some_and(|status| {
                status.state == crate::media::player::PlaybackState::Paused
            });
            let mut menu = Menu::builder()
                .append_item(
                    ID_PLAYLIST_PLAY,
                    &t!("Play this song\tEnter"),
                    &t!("Stop the song that is playing and play this one"),
                )
                .append_item(
                    ID_PLAYLIST_REMOVE,
                    &t!("Remove from playlist\tDelete"),
                    &t!("Take this song out of the playlist. The file is not deleted"),
                )
                .append_separator()
                .append_item(
                    ID_PLAYLIST_PAUSE,
                    &if paused { t!("Resume") } else { t!("Pause") },
                    &t!("Start or stop this media player"),
                )
                .append_item(
                    ID_PLAYLIST_OPEN,
                    &t!("Open file..."),
                    &t!("Play a file of your choosing, then carry on with the playlist"),
                )
                .append_item(
                    ID_PLAYLIST_FOLDER,
                    &t!("Choose folder..."),
                    &t!("Choose the folder this media player plays"),
                )
                .build();
            menu.enable_item(ID_PLAYLIST_PLAY, on_entry);
            menu.enable_item(ID_PLAYLIST_REMOVE, on_entry);
            // Anchored to the list rather than the mouse, so the keyboard
            // routes put the menu beside the control they came from.
            let at = field.list.client_to_screen(Point { x: 8, y: 8 });
            field.list.popup_menu(&mut menu, Some(at));
        });
    }
    {
        let app = app.clone();
        let field = field.clone();
        list.bind_with_id_internal(EventType::MENU, ID_PLAYLIST_PLAY, move |_| {
            play_selected(&app, &field);
        });
    }
    {
        let app = app.clone();
        let field = field.clone();
        list.bind_with_id_internal(EventType::MENU, ID_PLAYLIST_REMOVE, move |_| {
            remove_selected(&app, &field);
        });
    }
    {
        let app = app.clone();
        let source = field.source.clone();
        list.bind_with_id_internal(EventType::MENU, ID_PLAYLIST_PAUSE, move |_| {
            super::media_transport(&app, &source, Command::PlayPause);
        });
    }
    {
        let app = app.clone();
        let source = field.source.clone();
        list.bind_with_id_internal(EventType::MENU, ID_PLAYLIST_OPEN, move |_| {
            super::media_open_file(&app, &source);
        });
    }
    {
        let app = app.clone();
        let source = field.source.clone();
        list.bind_with_id_internal(EventType::MENU, ID_PLAYLIST_FOLDER, move |_| {
            super::media_choose_folder(&app, &source);
        });
    }
}

/// ALT+L: focus a playlist. From one playlist, the next one, so a scene with
/// two media players reaches both. The Home tab is brought forward first,
/// since that is where the lists live.
///
/// Arriving on a list with nothing selected would be read as an empty
/// selection, so the playing song is selected first — this is a deliberate
/// press, the one kind of thing that may move a selection (see `ui::list`).
pub fn focus_next(app: &Rc<App>) {
    let fields = fields(app);
    if fields.is_empty() {
        super::help::announce(&t!("There is no media player in this scene"));
        return;
    }
    let target = fields
        .iter()
        .position(|field| field.has_focus.get())
        .map_or(0, |at| (at + 1) % fields.len());
    let field = &fields[target];
    app.widgets(|w| {
        if w.notebook.selection() != 0 {
            w.notebook.set_selection(0);
        }
    });
    let rows = field.shown.borrow().paths.len();
    if rows > 0 && super::list::selection(&field.list, rows).is_none() {
        let playing = app
            .media
            .status(&field.source)
            .and_then(|status| status.playing)
            .filter(|&index| index < rows)
            .unwrap_or(0);
        field.list.set_selection(playing as u32, true);
        field.list.ensure_visible(playing as i32);
    }
    field.list.set_focus();
}

/// Every playlist in the mixer, for F6's ring of lists.
pub fn lists(app: &Rc<App>) -> Vec<ListBox> {
    fields(app).into_iter().map(|field| field.list).collect()
}

#[cfg(test)]
mod tests {
    use super::nearest;
    use std::path::PathBuf;

    #[test]
    fn the_selection_follows_the_song_to_its_nearest_copy() {
        let paths: Vec<PathBuf> = ["a", "b", "a", "c"].iter().map(PathBuf::from).collect();
        assert_eq!(nearest(&paths, &PathBuf::from("a"), 3), Some(2));
        assert_eq!(nearest(&paths, &PathBuf::from("a"), 0), Some(0));
        assert_eq!(nearest(&paths, &PathBuf::from("c"), 0), Some(3));
        assert_eq!(nearest(&paths, &PathBuf::from("z"), 0), None);
    }
}
