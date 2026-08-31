//! The `CGEventTap` behind a **global** keybinding on macOS.
//!
//! Every other key in the app arrives through the `NSEvent` local monitor in
//! [`super::help`], which sees only events sent to this application — exactly
//! right for F1, F6 and a binding meant to work while Pubsplash is in front. A
//! binding marked `global` is the one case that needs more: keys pressed while
//! *another* app has focus, so a broadcaster can mute a microphone without
//! leaving the game they are streaming.
//!
//! Only macOS charges for that, and the price is the **Accessibility
//! permission**. Three consequences shape this file.
//!
//! **It is asked for lazily, and only when a global binding exists.** A user who
//! never sets one is never prompted, because a permission dialog at launch for a
//! feature nobody is using is exactly the kind of thing that teaches people to
//! click Deny. [`refresh`] is called on every keybinds edit and decides from the
//! bindings themselves whether a tap should be running at all.
//!
//! **A refused permission is not an error.** It is a state: everything else
//! keeps working and global bindings behave like ordinary ones, which is what
//! they already do while the tap is absent. It is said once in the log rather
//! than in a modal, by the rule `CLAUDE.md` gives for notices that can arrive
//! unbidden.
//!
//! **The tap can be switched off by the system and must switch itself back on.**
//! If the callback ever takes too long, macOS disables the tap and sends
//! `TapDisabledByTimeout` instead of dropping it — so that event is handled, and
//! is the reason the callback does nothing but translate a key code and hand it
//! to the same [`super::keybinds::hook_key`] the local monitor calls.

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};
use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;

thread_local! {
    /// The running tap, if any. UI-thread-only, like everything else here.
    static TAP: RefCell<Option<CFRetained<CFMachPort>>> = const { RefCell::new(None) };
    /// Whether the permission has already been reported as missing, so the log
    /// says it once rather than on every keybinds edit.
    static REPORTED: RefCell<bool> = const { RefCell::new(false) };
}

/// Starts or stops the tap to match `wanted`.
///
/// Called from [`super::keybinds::reload`] with whether any binding is global,
/// so the tap exists exactly while it has something to do.
pub fn refresh(wanted: bool) {
    let running = TAP.with(|t| t.borrow().is_some());
    if wanted == running {
        return;
    }
    if wanted { start() } else { stop() }
}

/// The event types the tap asks for: key-down only, which is all
/// `hook_key` acts on.
fn mask() -> CGEventMask {
    1u64 << CGEventType::KeyDown.0
}

fn start() {
    // SAFETY: the callback matches `CGEventTapCallBack` and takes no user info.
    let tap = unsafe {
        CGEvent::tap_create(
            // Session rather than HID: this sees events after other session
            // taps and login-window handling, which is the level an ordinary
            // app belongs at.
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            // Not `ListenOnly`: a global binding has to be able to swallow its
            // key, or the app underneath acts on it too.
            CGEventTapOptions::Default,
            mask(),
            Some(callback),
            std::ptr::null_mut(),
        )
    };
    let Some(tap) = tap else {
        // The only reason `tap_create` fails in practice. Said once — see the
        // module header for why this is a log line and not a dialog.
        REPORTED.with(|reported| {
            if !*reported.borrow() {
                *reported.borrow_mut() = true;
                log::warn!(
                    "Global shortcuts need Accessibility permission for Pubsplash \
                     (System Settings > Privacy & Security > Accessibility). \
                     Until it is granted they work only while Pubsplash is in front."
                );
            }
        });
        return;
    };
    let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0);
    // The main run loop, in common modes, so the tap keeps working while a modal
    // dialog or a menu is up — those run the loop in a mode of their own, and a
    // tap added to the default mode alone would go deaf exactly when a user is
    // most likely to reach for a global shortcut.
    if let Some(run_loop) = CFRunLoop::main() {
        // SAFETY: reading a framework constant.
        run_loop.add_source(source.as_deref(), unsafe { kCFRunLoopCommonModes });
    }
    // Stored *before* it is enabled, because the callback re-enables the tap
    // through this very slot and the first event can arrive the moment it is on.
    TAP.with(|t| *t.borrow_mut() = Some(tap));
    with_tap(|tap| CGEvent::tap_enable(tap, true));
    log::info!("Global shortcuts are active");
}

fn stop() {
    let tap = TAP.with(|t| t.borrow_mut().take());
    if let Some(tap) = tap {
        // Disabled rather than only dropped: the run loop source holds a
        // reference, so letting go of ours is not enough to stop delivery.
        CGEvent::tap_enable(&tap, false);
    }
}

/// Runs `f` with the live tap port, if there is one.
///
/// The callback and everything else here are on the main thread — the tap is
/// added to the *main* run loop — so this thread-local is reachable from both.
fn with_tap(f: impl FnOnce(&CFMachPort)) {
    TAP.with(|t| {
        if let Some(tap) = t.borrow().as_ref() {
            f(tap);
        }
    });
}

/// The tap callback. Runs on the main run loop, and must be quick — see the
/// module header on `TapDisabledByTimeout`.
///
/// # Safety
/// `event` is the system's event for this callback.
unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    // macOS switches a slow or otherwise unhappy tap off and tells us rather
    // than dropping it. Turning it back on is the documented response, and
    // without it global shortcuts stop for the rest of the session.
    if event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput
    {
        with_tap(|tap| CGEvent::tap_enable(tap, true));
        log::debug!("The system switched the global shortcut tap off; switched it back on");
        return event.as_ptr();
    }
    if event_type != CGEventType::KeyDown {
        return event.as_ptr();
    }
    // SAFETY: the system owns this event for the duration of the call.
    let event_ref = unsafe { event.as_ref() };
    let code = CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventKeycode);
    let Ok(code) = u16::try_from(code) else {
        return event.as_ptr();
    };
    let Some(vk) = super::mac_keys::vk_for(code) else {
        return event.as_ptr();
    };
    // The same function the local monitor calls, so a global binding and an
    // ordinary one go through one gate — including the "is this a plain
    // character in a text box" check, which matters more here than there.
    if super::keybinds::hook_key(vk) {
        wxdragon::wake_up_idle();
        // Swallowed: returning null stops the key reaching the app in front.
        return std::ptr::null_mut();
    }
    event.as_ptr()
}
