//! Context-sensitive help: pressing **F1** on any control speaks a
//! hand-authored help string through the user's running screen reader.
//!
//! Three moving parts:
//!
//! 1. [`tag`] is called next to each control as it is built, stamping a stable
//!    help-id onto the control's native window. On Windows that is `SetPropW`,
//!    and the property dies with the window automatically, so rebuilt panels
//!    (the mixer) never leave stale ids behind — a property this file leans on
//!    rather than merely enjoys, which is why a side map keyed by window handle
//!    is the wrong shape for it.
//! 2. The authored messages live in `help.toml` at the repo root, embedded with
//!    `include_str!` and parsed once into an id -> message map. The dev binary
//!    `gen-help` round-trips that file from the `tag` call sites (see
//!    `src/bin/gen_help.rs`).
//! 3. A global keyboard hook (mirroring the F6 hook in `fx_editor`) catches F1
//!    app-wide, swallows it, and flags the pump. [`pump`] resolves the focused
//!    window to its id and announces the message through the screen reader — on
//!    Windows a UIA notification raised on the main frame's host provider, so
//!    NVDA and Narrator speak it, interrupting whatever they were saying.
//!
//! The authored content — `help.toml`, the parse, the interning — is portable
//! and stays here. Everything that stamps, hooks or speaks lives in `imp`, and
//! **none of it is built on macOS yet**; see the macOS `imp` for what each of
//! the three needs.
//!
//! The same hook also catches F6/SHIFT+F6 for [`super::panes`] and every user
//! keybinding for [`super::keybinds`], since it is the one hook installed for
//! the whole life of the app. The F6 arm fires only while
//! the *main frame itself* is foreground, so it can never collide with the
//! editor-local F6 in `fx_editor` (whose gate is a plugin editor frame being
//! foreground) or fire under a modal dialog.
//!
//! Threading: `tag`, `install_*`, `uninstall_*`, and `pump` all run on the UI
//! thread (thread-locals below live there); only the hook proc runs in the OS
//! hook context, and it touches nothing but atomics.

use crate::t;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize};

pub use imp::{
    announce, install_announcer, install_hook, pump, tag, uninstall_announcer, uninstall_hook,
};
pub(super) use imp::{foreground_is_main_frame, foreground_is_ours};

/// Spoken when a control has no id, or an id with no authored message.
fn generic() -> String {
    t!("No help available for this control.")
}

thread_local! {
    /// Interns help-ids to small indices so each can be stored as a single
    /// window-property value. Ids repeat (the mixer re-tags its strips on every
    /// rebuild), so the map dedupes and the vec stays tiny.
    static INTERN: RefCell<(Vec<String>, HashMap<String, usize>)> =
        RefCell::new((Vec::new(), HashMap::new()));
}

/// The installed low-level keyboard hook (as isize; 0 = not installed).
///
/// Windows only, and there is no macOS equivalent to hold here: an `NSEvent`
/// monitor is an *object* rather than a handle, so the macOS `imp` keeps it in a
/// thread-local of its own. Allowed to be unused off Windows rather than
/// `cfg`'d, so this file has one place where "what identifies the hook" is
/// written down for each platform.
#[cfg_attr(not(windows), allow(dead_code))]
static HELP_HOOK: AtomicIsize = AtomicIsize::new(0);
/// Set by the hook when F1 is pressed in our app; read and cleared by [`pump`].
static HELP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// The main frame's hwnd (as isize; 0 = not built yet), so the hook can tell it
/// apart from our dialogs and plugin editor frames. Set by [`install_announcer`],
/// which is handed the frame anyway.
static MAIN_FRAME: AtomicIsize = AtomicIsize::new(0);

fn intern(id: &str) -> usize {
    INTERN.with(|c| {
        let (vec, map) = &mut *c.borrow_mut();
        if let Some(&i) = map.get(id) {
            return i;
        }
        let i = vec.len();
        vec.push(id.to_string());
        map.insert(id.to_string(), i);
        i
    })
}

fn id_for(idx: usize) -> Option<String> {
    INTERN.with(|c| c.borrow().0.get(idx).cloned())
}

// --- authored content ------------------------------------------------------

const HELP_TOML: &str = include_str!("../../help.toml");

#[derive(serde::Deserialize, Default)]
struct HelpFile {
    #[serde(default)]
    control: Vec<HelpEntry>,
}

#[derive(serde::Deserialize)]
struct HelpEntry {
    id: String,
    #[serde(default)]
    message: String,
}

/// The id -> message map, parsed once. Entries with a blank message are dropped
/// so those controls fall back to [`generic()`].
fn messages() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| match toml::from_str::<HelpFile>(HELP_TOML) {
        Ok(file) => file
            .control
            .into_iter()
            .filter(|e| !e.message.trim().is_empty())
            .map(|e| (e.id, e.message))
            .collect(),
        Err(e) => {
            log::error!("help.toml parse error: {e}");
            HashMap::new()
        }
    })
}

/// The message for a stamped property value — index + 1, or 0/negative for a
/// control that was never tagged. Portable, so both `imp`s resolve the same way.
fn message_for_stamp(raw: isize) -> String {
    if raw <= 0 {
        return generic().to_string();
    }
    id_for((raw - 1) as usize)
        .and_then(|id| messages().get(&id).cloned())
        .unwrap_or_else(|| generic().to_string())
}

/// A window property for the stamp, a low-level keyboard hook for the keys, and
/// a server-side UIA provider on the frame for the speech.
///
/// The provider is not optional decoration. A screen reader only processes a UIA
/// notification whose provider is connected to its automation tree, and a bare
/// `UiaHostProviderFromHwnd` provider that was never returned through
/// `WM_GETOBJECT` is not connected — the notification is simply dropped. So a
/// real server-side provider is installed on the frame, reachable via the
/// frame's `WM_GETOBJECT`/`UiaRootObjectId`, and the notification is raised on
/// that. `HostRawElementProvider` delegates structure to the default window
/// provider, so the frame's children still enumerate normally.
#[cfg(windows)]
mod imp {
    use super::{HELP_REQUESTED, HELP_HOOK, MAIN_FRAME, intern};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        IRawElementProviderSimple, IRawElementProviderSimple_Impl, NotificationKind_Other,
        NotificationProcessing_ImportantMostRecent, ProviderOptions,
        ProviderOptions_ServerSideProvider, UIA_ControlTypePropertyId, UIA_NamePropertyId,
        UIA_PATTERN_ID, UIA_PROPERTY_ID, UIA_WindowControlTypeId, UiaClientsAreListening,
        UiaDisconnectProvider, UiaHostProviderFromHwnd, UiaRaiseNotificationEvent,
        UiaReturnRawElementProvider, UiaRootObjectId,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F1, VK_F6, VK_SHIFT};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetPropW,
        GetWindowThreadProcessId, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, SetPropW, SetWindowsHookExW,
        UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_GETOBJECT, WM_KEYDOWN, WM_SYSKEYDOWN,
    };
    use windows::core::{BSTR, Error, IUnknown, PCWSTR, Result, implement, w};
    use wxdragon::prelude::WxWidget;

    /// Distinguishes our frame subclass from any other on the same window.
    const FRAME_SUBCLASS_ID: usize = 0x0A12;

    /// Window-property name carrying a control's interned help-id (index + 1).
    const HELP_PROP: PCWSTR = w!("PubsplashHelpId");

    thread_local! {
        /// The main frame (hwnd) and the server-side UIA provider installed on
        /// it, used to raise notification events. Set by `install_announcer`.
        static ANNOUNCER: RefCell<Option<(isize, IRawElementProviderSimple)>> =
            const { RefCell::new(None) };
    }

    pub fn tag(widget: &dyn WxWidget, id: &str, _label: &str) {
        let hwnd = HWND(widget.get_handle());
        if hwnd.0.is_null() {
            return;
        }
        let handle = HANDLE((intern(id) as isize + 1) as *mut _);
        if let Err(e) = unsafe { SetPropW(hwnd, HELP_PROP, Some(handle)) } {
            log::warn!("help::tag: SetPropW failed for {id:?}: {e}");
        }
    }

    fn help_for(hwnd: HWND) -> String {
        super::message_for_stamp(unsafe { GetPropW(hwnd, HELP_PROP) }.0 as isize)
    }

    // --- UIA announcer ---------------------------------------------------------
    //
    // A screen reader only processes a UIA notification event whose provider is
    // connected to its automation tree. A bare `UiaHostProviderFromHwnd` provider
    // that was never returned through `WM_GETOBJECT` is not connected, so the
    // notification is dropped. We therefore install a real server-side provider on
    // the frame — reachable via the frame's `WM_GETOBJECT`/`UiaRootObjectId` — and
    // raise the notification on it. `HostRawElementProvider` delegates structure to
    // the default window provider, so the frame's children still enumerate normally.

    thread_local! {
        /// The frame provider, keyed by frame hwnd, for the subclass proc (which
        /// only has the hwnd) to return on the UIA root query.
        static FRAME_PROVIDER: RefCell<HashMap<isize, IRawElementProviderSimple>> =
            RefCell::new(HashMap::new());
    }

    #[implement(IRawElementProviderSimple)]
    struct FrameProvider {
        hwnd: isize,
    }

    impl IRawElementProviderSimple_Impl for FrameProvider_Impl {
        fn ProviderOptions(&self) -> Result<ProviderOptions> {
            Ok(ProviderOptions_ServerSideProvider)
        }

        fn GetPatternProvider(&self, _patternid: UIA_PATTERN_ID) -> Result<IUnknown> {
            // No control patterns; this element exists only to source notifications.
            Err(Error::empty())
        }

        fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> Result<VARIANT> {
            Ok(if propertyid == UIA_ControlTypePropertyId {
                VARIANT::from(UIA_WindowControlTypeId.0)
            } else if propertyid == UIA_NamePropertyId {
                VARIANT::from("Pubsplash")
            } else {
                VARIANT::default()
            })
        }

        fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
            // Delegate structure/navigation to the default window provider so the
            // frame's children remain visible to the screen reader.
            unsafe { UiaHostProviderFromHwnd(HWND(self.hwnd as *mut _)) }
        }
    }

    unsafe extern "system" fn frame_subclass(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _refdata: usize,
    ) -> LRESULT {
        if msg == WM_GETOBJECT
            && lparam.0 as i32 == UiaRootObjectId
            && let Some(provider) = FRAME_PROVIDER.with(|r| r.borrow().get(&(hwnd.0 as isize)).cloned())
        {
            return unsafe { UiaReturnRawElementProvider(hwnd, wparam, lparam, &provider) };
        }
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    /// Installs a UIA provider on the main frame so [`pump`] can raise notification
    /// events that the screen reader will speak, regardless of which control has
    /// focus.
    pub fn install_announcer(frame: &dyn WxWidget) {
        let hwnd = HWND(frame.get_handle());
        let key = hwnd.0 as isize;
        let provider: IRawElementProviderSimple = FrameProvider { hwnd: key }.into();
        FRAME_PROVIDER.with(|r| {
            r.borrow_mut().insert(key, provider.clone());
        });
        let ok = unsafe { SetWindowSubclass(hwnd, Some(frame_subclass), FRAME_SUBCLASS_ID, 0) };
        if !ok.as_bool() {
            log::warn!("help: SetWindowSubclass failed for frame hwnd={key:#x}");
        }
        ANNOUNCER.with(|a| *a.borrow_mut() = Some((key, provider)));
        MAIN_FRAME.store(key, Ordering::Relaxed);
    }

    pub fn uninstall_announcer() {
        MAIN_FRAME.store(0, Ordering::Relaxed);
        let entry = ANNOUNCER.with(|a| a.borrow_mut().take());
        if let Some((key, provider)) = entry {
            unsafe {
                let _ =
                    RemoveWindowSubclass(HWND(key as *mut _), Some(frame_subclass), FRAME_SUBCLASS_ID);
                if let Err(e) = UiaDisconnectProvider(&provider) {
                    log::warn!("help: UiaDisconnectProvider failed: {e}");
                }
            }
            FRAME_PROVIDER.with(|r| {
                r.borrow_mut().remove(&key);
            });
        }
    }

    /// Speaks `text` through the running screen reader immediately, interrupting
    /// whatever it was saying. Used for F1 help and for state changes that have no
    /// control of their own to announce them (see the mixer's monitoring toggle).
    pub fn announce(text: &str) {
        ANNOUNCER.with(|a| {
            let Some((_, provider)) = a.borrow().clone() else {
                return;
            };
            unsafe {
                if !UiaClientsAreListening().as_bool() {
                    return;
                }
                // ImportantMostRecent asks the screen reader to speak this now and
                // discard any older queued help, so repeated F1 presses interrupt
                // rather than pile up.
                if let Err(e) = UiaRaiseNotificationEvent(
                    &provider,
                    NotificationKind_Other,
                    NotificationProcessing_ImportantMostRecent,
                    &BSTR::from(text),
                    &BSTR::new(),
                ) {
                    log::warn!("help: UiaRaiseNotificationEvent failed: {e}");
                }
            }
        });
    }

    // --- F1 hook ---------------------------------------------------------------

    pub fn foreground_is_ours() -> bool {
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(fg, Some(&mut pid));
            pid == GetCurrentProcessId()
        }
    }

    /// True only while the main frame is the foreground window. Tighter than
    /// [`foreground_is_ours`] on purpose: it excludes our own modal dialogs and the
    /// plugin editor frames, which have their own F6 (see `fx_editor`).
    pub fn foreground_is_main_frame() -> bool {
        let frame = MAIN_FRAME.load(Ordering::Relaxed);
        frame != 0 && unsafe { GetForegroundWindow() }.0 as isize == frame
    }

    unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32
            && (wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN)
        {
            let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            // First refusal goes to the Add-binding dialog's shortcut box: while it
            // has focus every key belongs to it, F1 and F6 included, which is the
            // only way they could ever be typed into it.
            if super::super::keybinds::capture_key(kb.vkCode) {
                wxdragon::wake_up_idle();
                return LRESULT(1);
            }
            if kb.vkCode == VK_F1.0 as u32 && foreground_is_ours() {
                HELP_REQUESTED.store(true, Ordering::Relaxed);
                // Ring the doorbell: `pump` runs from the idle handler, and F1 is
                // swallowed below, so nothing else would wake the loop to notice.
                wxdragon::wake_up_idle();
                // Swallow F1 so no underlying control pops its own help.
                return LRESULT(1);
            }
            if kb.vkCode == VK_F6.0 as u32 && foreground_is_main_frame() {
                // Physical shift state: `GetKeyState` here would report this
                // thread's queued state, which is not what the user just held.
                let shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000 != 0;
                super::super::panes::request(shift);
                wxdragon::wake_up_idle();
                // Swallow it: MSW gives F6 its own pane-switching meaning.
                return LRESULT(1);
            }
            // Last, so the two built-ins above can never be shadowed by a user
            // binding. `keybinds_ui` refuses F1 and F6 for that reason.
            if super::super::keybinds::hook_key(kb.vkCode) {
                wxdragon::wake_up_idle();
                return LRESULT(1);
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// Installs the app-wide F1 hook. Idempotent.
    pub fn install_hook() {
        if HELP_HOOK.load(Ordering::Relaxed) != 0 {
            return;
        }
        unsafe {
            if let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
                HELP_HOOK.store(hook.0 as isize, Ordering::Relaxed);
            }
        }
    }

    /// Removes the F1 hook. Idempotent.
    pub fn uninstall_hook() {
        let raw = HELP_HOOK.swap(0, Ordering::Relaxed);
        if raw != 0 {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(raw as *mut _));
            }
        }
    }

    /// Called each pump tick. If F1 was pressed, resolves the focused control to its
    /// help message and announces it.
    pub fn pump() {
        if !HELP_REQUESTED.swap(false, Ordering::Relaxed) {
            return;
        }
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
        announce(&help_for(focus));
    }
}

/// A window property becomes an **associated object** on the control's
/// `NSView`, the low-level hook becomes an **`NSEvent` local monitor**, and the
/// UIA notification becomes an **`NSAccessibilityAnnouncementRequested`**
/// notification. Same three parts, three different mechanisms.
///
/// Two of those choices are load-bearing.
///
/// **The stamp is an associated object rather than a side map** for the reason
/// the module header gives for `SetPropW`: it dies with the view. A map keyed by
/// pointer would outlive the control, and the mixer destroys and rebuilds its
/// strips constantly — a fresh `NSView` allocated at a dead one's address would
/// inherit its help text, which is worse than having none.
///
/// **The monitor is *local*, which is why it needs no permission.** A local
/// monitor sees only events delivered to this app, which is exactly the gate
/// `foreground_is_ours` describes, so F1, F6 and every non-global binding work
/// with no prompt at all. A binding marked `global` needs a `CGEventTap` and the
/// Accessibility permission, and does not fire yet — see [`hook_key`]'s caller
/// in `keybinds`.
#[cfg(target_os = "macos")]
mod imp {
    use super::{HELP_REQUESTED, MAIN_FRAME, generic, intern, message_for_stamp};
    use objc2::ffi::{OBJC_ASSOCIATION_RETAIN_NONATOMIC, objc_getAssociatedObject,
        objc_setAssociatedObject};
    use objc2::Message;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification,
        NSAccessibilityPostNotificationWithUserInfo, NSAccessibilityPriorityKey,
        NSAccessibilityPriorityLevel, NSApplication, NSEvent, NSEventMask, NSEventModifierFlags,
        NSView, NSWindow,
    };
    use objc2_foundation::{MainThreadMarker, NSDictionary, NSNumber, NSString};
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::Ordering;
    use wxdragon::prelude::WxWidget;

    /// The association key. Only its *address* matters — Objective-C compares
    /// association keys by pointer — so this is a private static that nothing
    /// else in the process can collide with.
    static HELP_KEY: u8 = 0;

    thread_local! {
        /// The installed event monitor, so `uninstall_hook` can remove exactly
        /// the one it added. `NSEvent`'s monitor API is main-thread-only, which
        /// is where every function in this module runs.
        static MONITOR: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
    }

    /// The `NSView` behind a wx window, or `None` if it has no native peer yet.
    fn view_of(widget: &dyn WxWidget) -> Option<NonNull<AnyObject>> {
        // `get_handle` is `wxWindow::GetHandle`, which is the `NSView*` on this
        // platform.
        NonNull::new(widget.get_handle().cast::<AnyObject>())
    }

    /// Stamps `id` onto the control, as an index + 1 so that "never tagged"
    /// (a missing association) and "index 0" stay distinguishable — the same
    /// encoding `message_for_stamp` expects from the Windows property.
    pub fn tag(widget: &dyn WxWidget, id: &str, _label: &str) {
        let Some(view) = view_of(widget) else {
            return;
        };
        let stamp = NSNumber::new_isize(intern(id) as isize + 1);
        // SAFETY: `view` is a live `NSView`; the key is a private static
        // address; `RETAIN_NONATOMIC` makes the association own the number, and
        // it is released when the view is deallocated.
        unsafe {
            objc_setAssociatedObject(
                view.as_ptr(),
                std::ptr::from_ref(&HELP_KEY).cast::<c_void>(),
                Retained::into_raw(stamp).cast::<AnyObject>(),
                OBJC_ASSOCIATION_RETAIN_NONATOMIC,
            );
        }
    }

    /// The stamp on one view, or 0 for a view that was never tagged.
    fn stamp_of(view: &AnyObject) -> isize {
        // SAFETY: `view` is a live object and the key is the same static address
        // `tag` used; a view with no association answers null.
        let raw = unsafe {
            objc_getAssociatedObject(
                std::ptr::from_ref(view),
                std::ptr::from_ref(&HELP_KEY).cast::<c_void>(),
            )
        };
        let Some(raw) = NonNull::new(raw.cast_mut()) else {
            return 0;
        };
        // SAFETY: the only thing `tag` ever associates under this key is an
        // `NSNumber`, and the association keeps it alive as long as the view.
        let number: &NSNumber = unsafe { raw.cast::<NSNumber>().as_ref() };
        number.as_isize()
    }

    /// Walks up from the focused view until a tagged one is found.
    ///
    /// The walk is not defensive padding. A focused `NSTextField` hands first
    /// responder to a shared *field editor* — an `NSTextView` that is a child of
    /// the field and belongs to the window, not to the control — so the view the
    /// user is standing on is the parent of the one that has focus. Walking up
    /// answers both cases with one rule, and stops at the window's content view
    /// so an untagged control falls back to [`generic()`] rather than borrowing
    /// the frame's help.
    fn help_for_focus(window: &NSWindow) -> String {
        let Some(responder) = window.firstResponder() else {
            return generic();
        };
        let mut current: Option<Retained<NSView>> = responder.downcast::<NSView>().ok();
        while let Some(view) = current {
            let stamp = stamp_of(view.as_ref());
            if stamp > 0 {
                return message_for_stamp(stamp);
            }
            // SAFETY: reading a live view's superview.
            current = unsafe { view.superview() };
        }
        generic()
    }

    /// Remembers the frame's window, so announcements have somewhere to come
    /// from and `foreground_is_main_frame` has something to compare against.
    ///
    /// **The handle is asked what it is rather than assumed to be an `NSView`.**
    /// `wxWindow::GetHandle` is the view for an ordinary control, but wxOSX's
    /// top-level windows are a different peer type, and sending `window` to an
    /// `NSWindow` is an unrecognised selector — which is a crash, not a `None`.
    /// Both answers are accepted because either is a reasonable thing for wx to
    /// hand back, and neither is worth depending on.
    pub fn install_announcer(frame: &dyn WxWidget) {
        let Some(handle) = view_of(frame) else {
            return;
        };
        // SAFETY: the handle is a live Objective-C object; which class it is, is
        // exactly what the downcasts below establish.
        let object = unsafe { handle.as_ref() };
        let window = if let Some(window) = object.downcast_ref::<NSWindow>() {
            Some(window.retain())
        } else {
            object.downcast_ref::<NSView>().and_then(NSView::window)
        };
        let Some(window) = window else {
            log::warn!("The main frame has no native window; F1 help will not announce");
            return;
        };
        MAIN_FRAME.store(Retained::as_ptr(&window) as isize, Ordering::Relaxed);
    }

    pub fn uninstall_announcer() {
        MAIN_FRAME.store(0, Ordering::Relaxed);
    }

    /// The app's key window, which is what an announcement is posted from.
    fn announcing_window(mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
        let app = NSApplication::sharedApplication(mtm);
        app.keyWindow().or_else(|| app.mainWindow())
    }

    /// Speaks `text` through whatever screen reader is running.
    ///
    /// `NSAccessibilityPriorityLevel::High` is the counterpart of the Windows side's
    /// `ImportantMostRecent`: it makes VoiceOver interrupt what it was saying,
    /// which is what a user pressing F1 has asked for. Anything lower is queued
    /// behind the control's own announcement and arrives after the user has
    /// moved on.
    pub fn announce(text: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            // Every caller is on the UI thread; this is a guard, not a path.
            log::warn!("Dropped a screen-reader announcement raised off the UI thread: {text}");
            return;
        };
        let Some(window) = announcing_window(mtm) else {
            return;
        };
        let message = NSString::from_str(text);
        let priority = NSNumber::new_isize(NSAccessibilityPriorityLevel::High.0);
        let info = NSDictionary::from_slices(
            &[
                // SAFETY: framework constants, immutable and always present.
                unsafe { NSAccessibilityAnnouncementKey },
                unsafe { NSAccessibilityPriorityKey },
            ],
            &[message.as_ref() as &AnyObject, priority.as_ref() as &AnyObject],
        );
        // SAFETY: the window is live, the notification name is a framework
        // constant, and the dictionary holds exactly the two keys AppKit
        // documents for it.
        unsafe {
            NSAccessibilityPostNotificationWithUserInfo(
                window.as_ref(),
                NSAccessibilityAnnouncementRequestedNotification,
                Some(&info),
            );
        }
    }

    /// Installs the app-wide key monitor. Idempotent.
    ///
    /// The arm order below is the Windows hook's, and it is load-bearing for the
    /// same reason: capture first, so the Add-binding dialog's shortcut box can
    /// see F1 and F6 as typeable keys; then the two built-ins; then user
    /// bindings last, so a binding can never shadow F1 or F6. `keybinds_ui`
    /// refuses those two for exactly this reason.
    pub fn install_hook() {
        let already = MONITOR.with(|m| m.borrow().is_some());
        if already {
            return;
        }
        let handler = block2::RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit owns the event for the duration of the call.
            let event = unsafe { event.as_ref() };
            let (code, flags, repeat) = (event.keyCode(), event.modifierFlags(), event.isARepeat());
            if handle_key(code, flags, repeat) {
                // Swallowed: returning null stops the event reaching the control.
                return std::ptr::null_mut();
            }
            // Not ours: hand it straight back, unchanged.
            std::ptr::from_ref(event).cast_mut()
        });
        // SAFETY: the block is kept alive by the monitor object stored below,
        // and the mask names one event type AppKit supports.
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler)
        };
        MONITOR.with(|m| *m.borrow_mut() = monitor);
    }

    /// One key-down, answered with whether it was swallowed.
    ///
    /// Split out of the block so it reads as the list of arms it is, and so the
    /// modifier extraction is in one place.
    fn handle_key(code: u16, flags: NSEventModifierFlags, repeat: bool) -> bool {
        let Some(vk) = super::super::mac_keys::vk_for(code) else {
            return false;
        };
        // A held key repeats, and an action that fires per repeat is a mute
        // toggling forty times a second. Windows gets this for free because
        // `KBDLLHOOKSTRUCT` reports each repeat as a fresh key-down and the
        // actions are idempotent toggles driven from the pump; here the check is
        // explicit.
        if repeat {
            return false;
        }
        if super::super::keybinds::capture_key(vk) {
            wxdragon::wake_up_idle();
            return true;
        }
        if vk == crate::keybind::VK_F1 && flags_are_plain(flags) && foreground_is_ours() {
            HELP_REQUESTED.store(true, Ordering::Relaxed);
            // Ring the doorbell: `pump` runs from the idle handler and F1 is
            // swallowed, so nothing else would wake the loop to notice.
            wxdragon::wake_up_idle();
            return true;
        }
        // An open plugin editor gets F6 first: the key exists to get *out* of
        // the plugin's own interface, and a pane switch would leave the user
        // still inside it. This is what the Windows side's separate hook in
        // `fx_editor` does, arriving here instead because one monitor already
        // sees every key.
        if vk == crate::keybind::VK_F6 && super::super::fx_editor::escape_requested() {
            wxdragon::wake_up_idle();
            return true;
        }
        if vk == crate::keybind::VK_F6 && foreground_is_main_frame() {
            super::super::panes::request(flags.contains(NSEventModifierFlags::Shift));
            wxdragon::wake_up_idle();
            return true;
        }
        if super::super::keybinds::hook_key(vk) {
            wxdragon::wake_up_idle();
            return true;
        }
        false
    }

    /// Whether no modifier the user could have meant is held.
    ///
    /// F1 is the help key on its own; CTRL+F1 or OPTION+F1 may be somebody's
    /// binding, and swallowing those here would shadow it. Caps Lock, the
    /// numeric-keypad flag and the function-key flag are ignored: macOS sets
    /// `Function` on every F-key press, so testing the whole word would mean F1
    /// never matched at all.
    fn flags_are_plain(flags: NSEventModifierFlags) -> bool {
        !flags.contains(NSEventModifierFlags::Control)
            && !flags.contains(NSEventModifierFlags::Option)
            && !flags.contains(NSEventModifierFlags::Command)
            && !flags.contains(NSEventModifierFlags::Shift)
    }

    /// Removes the key monitor. Idempotent.
    pub fn uninstall_hook() {
        let monitor = MONITOR.with(|m| m.borrow_mut().take());
        if let Some(monitor) = monitor {
            // SAFETY: this is the object `addLocalMonitor...` returned, removed
            // once.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    }

    pub fn pump() {
        if !HELP_REQUESTED.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // The window the user is actually typing into, which under a modal
        // dialog is the dialog rather than the frame.
        let Some(window) = app.keyWindow() else {
            announce(&generic());
            return;
        };
        announce(&help_for_focus(&window));
    }

    /// Whether Pubsplash is the active application.
    ///
    /// A *local* event monitor only ever sees events sent to this app, so by the
    /// time [`handle_key`] runs this is very nearly always true. It is asked
    /// anyway, because it is also called from `keybinds` to gate a non-global
    /// binding, and that path has to keep meaning what it says.
    pub fn foreground_is_ours() -> bool {
        MainThreadMarker::new().is_some_and(|mtm| NSApplication::sharedApplication(mtm).isActive())
    }

    /// Whether the main frame — not one of our dialogs, and not a plugin editor
    /// — is the window taking keys.
    ///
    /// This is what keeps F6 and non-global bindings from firing under a modal
    /// dialog while the pump timer keeps ticking, which is the same job the
    /// Windows side's foreground-window comparison does.
    pub fn foreground_is_main_frame() -> bool {
        let frame = MAIN_FRAME.load(Ordering::Relaxed);
        if frame == 0 {
            return false;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        app.keyWindow()
            .is_some_and(|window| Retained::as_ptr(&window) as isize == frame)
    }
}
