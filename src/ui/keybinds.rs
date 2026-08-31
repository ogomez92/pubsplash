//! Runtime half of user-configurable shortcuts: catching the keys and running
//! the actions. The data model is [`crate::keybind`], the settings UI is
//! [`super::keybinds_ui`].
//!
//! ## Why the low-level hook
//!
//! There is no wx accelerator table in this app, and adding one would not help:
//! an accelerator only fires while our window has focus, and a control that
//! wants raw keys (the chat input, a mixer slider) eats them first. Worse, a
//! **global** binding is not something wx can express at all.
//!
//! So both capture and dispatch ride the app-wide keyboard hook that
//! [`super::help`] already installs for F1 — that file is explicit that it is
//! "the one hook installed for the whole life of the app", and it already hosts
//! the F6 arm for [`super::panes`]. The hook calls into here in a fixed order:
//! capture first (while the Add-binding dialog's shortcut box has focus, keys
//! belong to it and nothing else), then F1, then F6, then dispatch. F1 and F6
//! are consequently refused as bindings by the dialog rather than being accepted
//! and then silently shadowed. That order is the same on both platforms, and for
//! the same reason; only the hook underneath differs — a `WH_KEYBOARD_LL` hook
//! on Windows, an `NSEvent` local monitor on macOS.
//!
//! Riding the hook is also what makes the capture box work at all: ESC, ENTER,
//! TAB, the arrows and CTRL+letter are all swallowed by `::IsDialogMessage`
//! during wx's message *pre-processing*, before any handler on the control can
//! see them (`ui/slider_uia.rs` documents this at length). The hook runs ahead
//! of all of that.
//!
//! ## Threading
//!
//! The hook proc must touch nothing but atomics and its own mutexes — it runs in
//! the OS hook context and any `RefCell` borrow of `App` live elsewhere at that
//! moment would panic. So the hook matches against a flat [`SNAPSHOT`] refreshed
//! by [`reload`] on every keybinds edit, parks the matched action in [`PENDING`],
//! rings the idle doorbell, and returns; [`pump`] does the real work on the UI
//! thread from the 100 ms pump. Same shape as `panes.rs`.
//!
//! ## Chords are Windows virtual-key codes, on both platforms
//!
//! Not wx key codes, and not scan codes — the settings file stores raw VK
//! numbers, and has since before there was a second platform. That is why the
//! codes below are spelled as literals rather than imported from the `windows`
//! crate: it makes the format explicit instead of implicit, and it means the
//! matching logic compiles anywhere.
//!
//! macOS reports something else entirely — an `NSEvent.keyCode` is a *position*
//! on the keyboard, with no arithmetic relation to a VK number — and **the
//! decision taken was to translate at the hook rather than to change the
//! model.** [`super::mac_keys`] is that translation and explains why: the number
//! is a wire format, a settings file has to mean the same thing on either
//! machine, and translating at the one point where a real keystroke enters the
//! app leaves this file, [`crate::keybind`] and all their tests portable and
//! untouched. The alternative — a portable key enum — would have meant migrating
//! every existing settings file to buy nothing a user could see.
//!
//! ## What a `global` binding does on macOS
//!
//! An `NSEvent` local monitor sees only events sent to this app, which is
//! exactly right for every binding except a global one. **A binding marked
//! global therefore behaves like a non-global one there**: it works while
//! Pubsplash is in front and does nothing while another app is. Reaching keys
//! meant for another application needs a `CGEventTap` and the Accessibility
//! permission, which is the remaining piece; the gating in [`hook_key`] is
//! already written for it, so the tap has only to call the same function.

use crate::t;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

/// Two questions only the OS can answer, and only it can: the hook runs outside
/// the wx event loop with no `App` to borrow.
///
/// - **modifiers** is the *physical* state, not this thread's queued state.
/// - **focus_is_text_entry** keeps a binding on a bare character key (`K`, `7`)
///   from making that key untypable in the chat box — the hook swallows it
///   before the control ever sees it. Anything with a modifier, and every
///   non-character key, still fires.
use imp::{focus_is_text_entry, modifiers};

use super::App;
use super::home::{self, StripTarget};
use crate::config::Config;
use crate::keybind::{BindAction, Chord};

/// Every binding, flattened for the hook. Rebuilt by [`reload`].
static SNAPSHOT: OnceLock<Mutex<Vec<(Chord, bool, BindAction)>>> = OnceLock::new();
/// Actions whose key has been pressed but which have not run yet.
static PENDING: OnceLock<Mutex<Vec<BindAction>>> = OnceLock::new();

fn snapshot() -> &'static Mutex<Vec<(Chord, bool, BindAction)>> {
    SNAPSHOT.get_or_init(|| Mutex::new(Vec::new()))
}

fn pending() -> &'static Mutex<Vec<BindAction>> {
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

/// Re-reads the bindings the hook matches against. Call after any edit to
/// `config.keybinds`, and once at startup.
pub fn reload(config: &Config) {
    let binds = config
        .keybinds
        .binds
        .iter()
        .map(|b| (b.key, b.global, b.action.clone()))
        .collect();
    // Poisoning here would mean a panic inside the hook; keep running with what
    // we have rather than taking the app down over a shortcut table.
    *snapshot().lock().unwrap_or_else(|e| e.into_inner()) = binds;

    // The tap that reaches keys meant for other applications exists exactly
    // while some binding is global, so a user who sets none is never asked for
    // the Accessibility permission. Windows needs nothing here: its one
    // low-level hook is already global.
    #[cfg(target_os = "macos")]
    super::global_keys::refresh(config.keybinds.binds.iter().any(|b| b.global));
}

// --- capture ---------------------------------------------------------------

/// Set while the Add-binding dialog's shortcut box has keyboard focus.
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// The chord the user last pressed while capturing, packed by [`pack`].
static CAPTURED: AtomicU32 = AtomicU32::new(0);
/// Whether [`CAPTURED`] holds something [`pump`] has not shown yet.
static CAPTURED_NEW: AtomicBool = AtomicBool::new(false);

fn pack(chord: Chord) -> u32 {
    chord.vk & 0xFF
        | (chord.ctrl as u32) << 8
        | (chord.alt as u32) << 9
        | (chord.shift as u32) << 10
}

fn unpack(bits: u32) -> Chord {
    Chord {
        vk: bits & 0xFF,
        ctrl: bits & 0x100 != 0,
        alt: bits & 0x200 != 0,
        shift: bits & 0x400 != 0,
    }
}

pub fn begin_capture() {
    CAPTURED_NEW.store(false, Ordering::Relaxed);
    CAPTURING.store(true, Ordering::Relaxed);
}

pub fn end_capture() {
    CAPTURING.store(false, Ordering::Relaxed);
}

/// The chord captured since the last call, if any. Drains the flag.
pub fn take_captured() -> Option<Chord> {
    CAPTURED_NEW
        .swap(false, Ordering::Relaxed)
        .then(|| unpack(CAPTURED.load(Ordering::Relaxed)))
}

/// The virtual-key codes this module names. See the module header for why they
/// are literals: a chord in the settings file is a raw VK number on both
/// platforms.
const VK_TAB: u32 = 0x09;
const VK_ESCAPE: u32 = 0x1B;
const VK_DELETE: u32 = 0x2E;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
/// ALT.
const VK_MENU: u32 = 0x12;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

fn is_modifier_key(vk: u32) -> bool {
    matches!(vk, VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN)
        // The left/right-specific codes, which a hook reports in place of the
        // generic ones on some keyboards.
        || matches!(vk, 0xA0..=0xA5)
}

/// Called from the hook before anything else. Returns `true` to swallow the key.
///
/// TAB is deliberately passed through: the shortcut box must stay escapable with
/// TAB and SHIFT+TAB, which is the whole reason a screen-reader user can leave
/// it at all.
pub fn capture_key(vk: u32) -> bool {
    if !CAPTURING.load(Ordering::Relaxed) {
        return false;
    }
    // Alt-tabbing away does not necessarily fire the box's kill-focus, and
    // swallowing every keystroke in whatever the user switched to would be the
    // worst bug this feature could have.
    if !super::help::foreground_is_ours() {
        return false;
    }
    if vk == VK_TAB {
        return false;
    }
    if is_modifier_key(vk) {
        // A modifier on its own is not a chord — but it must be *passed through*,
        // not swallowed.
        //
        // Returning 1 from a `WH_KEYBOARD_LL` hook stops the key being processed
        // any further, and that includes the async key-state table. Swallowing
        // the CTRL keydown here therefore meant `GetAsyncKeyState(VK_CONTROL)`
        // still read "up" when the letter arrived a moment later, and every
        // chord captured as a bare character with no modifiers on it.
        return false;
    }
    let chord = if vk == VK_ESCAPE || vk == VK_DELETE {
        // Both clear the binding. ESC in particular must not reach the dialog,
        // or it would close it instead.
        Chord::default()
    } else {
        let (ctrl, alt, shift) = modifiers();
        Chord::new(vk, ctrl, alt, shift)
    };
    CAPTURED.store(pack(chord), Ordering::Relaxed);
    CAPTURED_NEW.store(true, Ordering::Relaxed);
    true
}

// --- dispatch --------------------------------------------------------------

/// Called from the hook. Returns `true` when a binding claimed the key, in which
/// case the caller swallows it and wakes the idle pump.
pub fn hook_key(vk: u32) -> bool {
    if is_modifier_key(vk) {
        return false;
    }
    let (ctrl, alt, shift) = modifiers();
    let pressed = Chord::new(vk, ctrl, alt, shift);

    let action = {
        let binds = snapshot().lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, global, action)) = binds.iter().find(|(chord, _, _)| *chord == pressed) else {
            return false;
        };
        // Gating. A non-global binding fires only while the *main frame itself*
        // is foreground, which excludes our own modal dialogs and the plugin
        // editor frames — the pump timer keeps firing inside a modal loop, and
        // some actions open dialogs of their own. A global binding is allowed
        // the same case plus "we are not in the foreground at all"; under our
        // own modal it stays silent for exactly the same reason.
        let ours = super::help::foreground_is_ours();
        let main = super::help::foreground_is_main_frame();
        let allowed = if *global { main || !ours } else { main };
        if !allowed {
            return false;
        }
        if pressed.is_plain_character() && focus_is_text_entry() {
            return false;
        }
        action.clone()
    };

    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(action);
    true
}

/// Called each pump tick. One uncontended lock unless a binding actually fired.
pub fn pump(app: &Rc<App>) {
    let actions: Vec<BindAction> = {
        let mut queue = pending().lock().unwrap_or_else(|e| e.into_inner());
        if queue.is_empty() {
            return;
        }
        std::mem::take(&mut *queue)
    };
    if app.shutting_down.get() {
        // Widgets are being torn down; touching them would be a use-after-free.
        return;
    }
    for action in actions {
        run(app, &action);
    }
}

// --- the actions -----------------------------------------------------------

/// Resolves a source identity name to its index in the **active** scene.
///
/// Bindings store the identity name rather than an index because the index means
/// nothing outside one scene. A name that is not in the current scene is not an
/// error — the user is simply somewhere else — so it announces and stops.
fn source_target(app: &Rc<App>, name: &str) -> Option<StripTarget> {
    let index = app
        .config
        .borrow()
        .scenes
        .active_scene()
        .and_then(|scene| scene.sources.iter().position(|s| s.name == name));
    match index {
        Some(index) => Some(StripTarget::Source(index)),
        None => {
            super::help::announce(&t!("{name} is not in the current scene", name = name));
            None
        }
    }
}

fn bus_target(app: &Rc<App>, name: &str) -> Option<StripTarget> {
    let index = app
        .config
        .borrow()
        .buses
        .buses
        .iter()
        .position(|b| b.name == name);
    match index {
        Some(index) => Some(StripTarget::Bus(index)),
        None => {
            super::help::announce(&t!("There is no bus called {name}", name = name));
            None
        }
    }
}

fn run(app: &Rc<App>, action: &BindAction) {
    match action {
        BindAction::ToggleStream => {
            if app.schedule_armed() {
                // Matches the button, which reads "Cancel scheduled stream"
                // while a schedule is waiting to go live.
                super::schedule_ui::cancel(app);
            } else if app.is_streaming_or_starting() {
                app.stop_streaming();
            } else if app.run.borrow().recording {
                // Same rule the disabled stream button expresses: streaming and a
                // standalone recording are mutually exclusive.
                super::help::announce(&t!("Cannot stream while a recording is running"));
            } else {
                super::start_streaming(app);
            }
        }
        BindAction::ToggleRecording => {
            let recording = app.run.borrow().recording;
            if recording {
                app.stop_recording();
            } else if app.is_streaming_or_starting() {
                super::help::announce(&t!("Cannot start a recording while streaming"));
            } else if app.schedule_armed() {
                // Same rule the disabled record button expresses while a schedule
                // is armed: a recording running when it fires would block the
                // stream it was armed for.
                super::help::announce(&t!("Cannot start a recording while a stream is scheduled"));
            } else {
                app.start_recording();
            }
        }
        BindAction::NextScene => home::cycle_scene(app, true),
        BindAction::PreviousScene => home::cycle_scene(app, false),
        BindAction::SwitchScene { scene } => home::switch_to_scene_named(app, scene),
        BindAction::ToggleMonitorMaster => home::toggle_monitor_target(app, StripTarget::Master),
        BindAction::ToggleMonitorSource { source } => {
            if let Some(target) = source_target(app, source) {
                home::toggle_monitor_target(app, target);
            }
        }
        BindAction::ToggleMonitorBus { bus } => {
            if let Some(target) = bus_target(app, bus) {
                home::toggle_monitor_target(app, target);
            }
        }
        BindAction::ToggleMuteMaster => home::toggle_mute_target(app, StripTarget::Master),
        BindAction::ToggleMuteSource { source } => {
            if let Some(target) = source_target(app, source) {
                home::toggle_mute_target(app, target);
            }
        }
        BindAction::ToggleMuteBus { bus } => {
            if let Some(target) = bus_target(app, bus) {
                home::toggle_mute_target(app, target);
            }
        }
        BindAction::ToggleMediaPlayback { source } => {
            super::media_transport(app, source, crate::media::player::Command::PlayPause);
        }
        BindAction::NextTrack { source } => {
            super::media_transport(app, source, crate::media::player::Command::Next);
        }
        BindAction::OpenTrack { source } => super::media_open_file(app, source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_survive_the_atomic_packing() {
        for chord in [
            Chord::default(),
            Chord::plain(0x78),
            Chord::new(0xFF, true, true, true),
            Chord::new(b'M' as u32, true, false, true),
            Chord::new(b'K' as u32, false, true, false),
        ] {
            assert_eq!(unpack(pack(chord)), chord);
        }
    }

    #[test]
    fn the_modifier_keys_are_never_chords_of_their_own() {
        assert!(is_modifier_key(0xA0)); // left shift
        assert!(is_modifier_key(0xA5)); // right alt
        assert!(is_modifier_key(VK_CONTROL));
        assert!(is_modifier_key(VK_LWIN));
        assert!(!is_modifier_key(0x78)); // F9
        assert!(!is_modifier_key(b'K' as u32));
    }
}

/// `GetAsyncKeyState` for the modifiers, and the focused window's class name for
/// the text-entry test.
#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GUITHREADINFO, GetClassNameW, GetGUIThreadInfo};

    /// Physical modifier state. `GetKeyState` would report this thread's
    /// *queued* state, which is not what the user is holding right now —
    /// `help.rs` makes the same point about its SHIFT read.
    pub fn modifiers() -> (bool, bool, bool) {
        let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
        (down(VK_CONTROL.0), down(VK_MENU.0), down(VK_SHIFT.0))
    }

    pub fn focus_is_text_entry() -> bool {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let focus = unsafe {
            if GetGUIThreadInfo(0, &mut info).is_ok() {
                info.hwndFocus
            } else {
                HWND::default()
            }
        };
        if focus.0.is_null() {
            return false;
        }
        let mut buffer = [0u16; 64];
        let written = unsafe { GetClassNameW(focus, &mut buffer) };
        if written <= 0 {
            return false;
        }
        let class = String::from_utf16_lossy(&buffer[..written as usize]);
        let class = class.to_ascii_lowercase();
        class == "edit" || class.starts_with("richedit") || class == "combobox"
    }
}

/// The two questions the hook asks about the world around a keystroke.
///
/// **Neither uses the accessibility API, and that is the point**: both are
/// answered out of AppKit, so nothing here needs the Accessibility permission.
/// The seam originally guessed `focus_is_text_entry` would have to ask the AX
/// API for the focused element's role; it does not, because the app can simply
/// look at its own first responder — which is both cheaper and available with no
/// prompt at all.
#[cfg(target_os = "macos")]
mod imp {
    use objc2_app_kit::{
        NSApplication, NSComboBox, NSEvent, NSEventModifierFlags, NSSearchField, NSText,
        NSTextField, NSTextView,
    };
    use objc2_foundation::MainThreadMarker;

    /// Which of CTRL, ALT and SHIFT are physically held.
    ///
    /// `NSEvent::modifierFlags` is a *class* method reporting the current state
    /// of the hardware, which is what `GetAsyncKeyState` gives the Windows side
    /// — not the flags of some queued event. That distinction matters for the
    /// same reason it does there: what is wanted is what the user is holding
    /// now.
    ///
    /// ALT is macOS's Option key, which is what the Windows `VK_MENU` maps onto.
    /// Command is deliberately not folded into any of the three: it is a
    /// distinct modifier, and reporting it as CTRL would make ⌘M fire a binding
    /// set for CTRL+M.
    pub fn modifiers() -> (bool, bool, bool) {
        let flags = NSEvent::modifierFlags_class();
        (
            flags.contains(NSEventModifierFlags::Control),
            flags.contains(NSEventModifierFlags::Option),
            flags.contains(NSEventModifierFlags::Shift),
        )
    }

    /// Whether the user is typing into a text field.
    ///
    /// This is what stops a plain-character binding — the `O` a new Media Player
    /// is given, say — from firing while somebody types a stream title. The
    /// Windows side answers it by window class name; here it is the first
    /// responder's class.
    ///
    /// **`NSTextView` is the important one, not `NSTextField`.** A focused text
    /// field does not become first responder itself: the window hands focus to a
    /// shared *field editor*, an `NSTextView` it owns, and leaves the field as
    /// that editor's delegate. So a check for `NSTextField` alone would answer
    /// "no" for every ordinary text box in the app. The others are listed
    /// because a control can be first responder in its own right before its
    /// editor is installed, and answering "yes" too often costs a binding that
    /// the user can still reach by moving focus, where answering "no" too often
    /// corrupts what they are typing.
    pub fn focus_is_text_entry() -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        let Some(window) = app.keyWindow() else {
            return false;
        };
        let Some(responder) = window.firstResponder() else {
            return false;
        };
        responder.downcast_ref::<NSTextView>().is_some()
            || responder.downcast_ref::<NSText>().is_some()
            || responder.downcast_ref::<NSTextField>().is_some()
            || responder.downcast_ref::<NSComboBox>().is_some()
            || responder.downcast_ref::<NSSearchField>().is_some()
    }
}
