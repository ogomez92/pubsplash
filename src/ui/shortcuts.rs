//! The main window's built-in shortcuts — the ones that are not user
//! keybindings and are not on a menu:
//!
//! - **CTRL+1** to **CTRL+5** bring a tab forward (Home, Chat, Scenes and
//!   Sources, Buses, API) and put focus on it.
//! - **ALT+1** to **ALT+0** read out the latest ten chat messages, ALT+1 the
//!   newest and ALT+0 the tenth newest, without leaving wherever the user is.
//! - **ALT+L** focuses a media player's playlist ([`super::playlist`]).
//!
//! They ride the one keyboard hook (`help::keyboard_hook`, and the `NSEvent`
//! monitor on macOS) through [`super::keybinds::hook_key`], and they are asked
//! **after** the user's own bindings: a user who has bound one of these chords
//! to something else keeps their binding. That is the opposite of F1 and F6,
//! which can never be shadowed because they are the way out of places — these
//! are conveniences, and a user binding is a choice somebody made.
//!
//! Like every non-global binding they fire only while the main frame is the
//! foreground window, so they stay out of dialogs. The hook touches only the
//! queue below; everything runs from [`pump`] on the UI thread.

use crate::keybind::Chord;
use crate::t;
use crate::tn;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::App;
use wxdragon::prelude::WxWidget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    /// Bring this tab forward, counting from zero.
    Tab(usize),
    /// Read the chat message this many back from the newest, counting from
    /// one.
    ChatMessage(usize),
    FocusPlaylist,
}

const VK_0: u32 = 0x30;
const VK_9: u32 = 0x39;
const VK_L: u32 = b'L' as u32;

/// How many tabs the main window has, so CTRL+9 on a five-tab window is left
/// alone rather than swallowed for nothing. Set once the notebook is built.
static PAGE_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn set_page_count(count: usize) {
    PAGE_COUNT.store(count, Ordering::Relaxed);
}

/// Which shortcut a chord is, if any. Pure, so the table can be tested.
///
/// The digits are the main row only. The keypad's are left alone on purpose:
/// ALT plus keypad digits is how Windows types a character by its code.
pub fn shortcut_for(chord: Chord, pages: usize) -> Option<Shortcut> {
    let digit = (VK_0..=VK_9)
        .contains(&chord.vk)
        .then(|| (chord.vk - VK_0) as usize);
    match (chord.ctrl, chord.alt, chord.shift) {
        (true, false, false) => match digit {
            Some(digit @ 1..=9) if digit <= pages => Some(Shortcut::Tab(digit - 1)),
            _ => None,
        },
        (false, true, false) => match digit {
            Some(0) => Some(Shortcut::ChatMessage(10)),
            Some(digit) => Some(Shortcut::ChatMessage(digit)),
            None if chord.vk == VK_L => Some(Shortcut::FocusPlaylist),
            None => None,
        },
        _ => None,
    }
}

fn pending() -> &'static Mutex<Vec<Shortcut>> {
    static PENDING: Mutex<Vec<Shortcut>> = Mutex::new(Vec::new());
    &PENDING
}

/// Called from the hook, after the user's bindings have declined the key.
/// Returns `true` when the key is one of these, in which case it is swallowed.
pub fn hook_chord(chord: Chord) -> bool {
    let Some(shortcut) = shortcut_for(chord, PAGE_COUNT.load(Ordering::Relaxed)) else {
        return false;
    };
    if !super::help::foreground_is_main_frame() {
        return false;
    }
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(shortcut);
    true
}

/// Called each pump tick. One uncontended lock unless a shortcut fired.
pub fn pump(app: &Rc<App>) {
    let shortcuts: Vec<Shortcut> = {
        let mut queue = pending().lock().unwrap_or_else(|e| e.into_inner());
        if queue.is_empty() {
            return;
        }
        std::mem::take(&mut *queue)
    };
    if app.shutting_down.get() {
        return;
    }
    for shortcut in shortcuts {
        match shortcut {
            Shortcut::Tab(index) => show_tab(app, index),
            Shortcut::ChatMessage(back) => read_chat_message(app, back),
            Shortcut::FocusPlaylist => super::playlist::focus_next(app),
        }
    }
}

/// Brings a tab forward and focuses it. A page's `SetFocus` hands focus on to
/// the control last focused on it (or its first), so coming back to a tab
/// returns the user to where they were.
fn show_tab(app: &Rc<App>, index: usize) {
    app.widgets(|w| {
        if index >= w.notebook.get_page_count() {
            return;
        }
        if w.notebook.selection() != index as i32 {
            w.notebook.set_selection(index);
        }
        if let Some(page) = w.notebook.get_page(index) {
            page.set_focus();
        }
    });
}

/// Reads one recent chat message aloud, `back` messages from the newest.
fn read_chat_message(app: &Rc<App>, back: usize) {
    let spoken = {
        let run = app.run.borrow();
        message_to_speak(&run.chat, back)
    };
    super::help::announce(&spoken);
}

/// What ALT+digit says. Split out so it can be tested without a window.
fn message_to_speak(chat: &[super::ChatEntry], back: usize) -> String {
    if chat.is_empty() {
        return t!("No chats");
    }
    match chat.len().checked_sub(back).and_then(|index| chat.get(index)) {
        Some(entry) => t!(
            "{message}, {age}",
            message = entry.prefix,
            age = crate::state::relative_time(entry.received.elapsed())
        ),
        None => tn!(
            "There is only {n} chat message",
            "There are only {n} chat messages",
            chat.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybind::Chord;

    const fn ctrl(vk: u32) -> Chord {
        Chord {
            vk,
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    const fn alt(vk: u32) -> Chord {
        Chord {
            vk,
            ctrl: false,
            alt: true,
            shift: false,
        }
    }

    #[test]
    fn ctrl_digits_are_tabs_up_to_the_number_there_are() {
        assert_eq!(shortcut_for(ctrl(0x31), 5), Some(Shortcut::Tab(0)));
        assert_eq!(shortcut_for(ctrl(0x35), 5), Some(Shortcut::Tab(4)));
        assert_eq!(shortcut_for(ctrl(0x36), 5), None, "there is no sixth tab");
        assert_eq!(shortcut_for(ctrl(0x30), 5), None);
    }

    #[test]
    fn alt_digits_are_the_last_ten_messages_with_zero_as_ten() {
        assert_eq!(shortcut_for(alt(0x31), 5), Some(Shortcut::ChatMessage(1)));
        assert_eq!(shortcut_for(alt(0x39), 5), Some(Shortcut::ChatMessage(9)));
        assert_eq!(shortcut_for(alt(0x30), 5), Some(Shortcut::ChatMessage(10)));
        assert_eq!(shortcut_for(alt(b'L' as u32), 5), Some(Shortcut::FocusPlaylist));
    }

    #[test]
    fn other_modifiers_are_left_alone() {
        let mut chord = ctrl(0x31);
        chord.shift = true;
        assert_eq!(shortcut_for(chord, 5), None);
        let mut chord = alt(0x31);
        chord.ctrl = true;
        assert_eq!(shortcut_for(chord, 5), None, "CTRL+ALT is AltGr on many layouts");
        assert_eq!(shortcut_for(Chord::plain(0x31), 5), None);
        assert_eq!(shortcut_for(alt(b'K' as u32), 5), None);
    }

    #[test]
    fn alt_digit_reads_back_from_the_newest() {
        let chat: Vec<super::super::ChatEntry> = ["one", "two", "three"]
            .iter()
            .map(|text| super::super::ChatEntry::new("ana".into(), text.to_string()))
            .collect();
        assert!(message_to_speak(&chat, 1).starts_with("ana: three, "));
        assert!(message_to_speak(&chat, 3).starts_with("ana: one, "));
        assert_eq!(message_to_speak(&chat, 4), "There are only 3 chat messages");
        assert_eq!(message_to_speak(&chat[..1], 2), "There is only 1 chat message");
        assert_eq!(message_to_speak(&[], 1), "No chats");
    }
}
