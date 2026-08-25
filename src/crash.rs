//! Turns a hard crash into something readable.
//!
//! Pubsplash hosts third-party plugin code in-process, and when that code faults
//! the process dies from a *hardware* fault, not a Rust panic — an access
//! violation on Windows, a `SIGSEGV` or `SIGBUS` on macOS. There is no unwind,
//! no panic hook, no message and no backtrace, so `logging::install_panic_hook`
//! never sees it and the log file simply stops mid-sentence. That is exactly the
//! situation where the one fact worth having is *whose code faulted*.
//!
//! So this installs a top-level fault handler that writes, before the process
//! goes away:
//!
//! - what the fault was, and the address it happened at;
//! - the **module that address belongs to**, and the offset within it — which
//!   names the plugin binary, `vst3-host`, or us, and is what turns "it crashes
//!   when I delete an effect" into a fixable bug;
//! - where a full dump of the process can be found.
//!
//! The handler runs on the faulting thread with a possibly-corrupt heap, so it
//! does as little as it can get away with and guards against re-entering itself.
//!
//! [`module_for`] is the part that matters and is the same idea on both
//! platforms — `GetModuleHandleExW` from an address, or `dladdr` — so it has one
//! signature and one set of tests. Everything around it differs: Windows has a
//! single `SetUnhandledExceptionFilter` and writes its own minidump, while macOS
//! takes a `sigaction` per fatal signal and leaves the dump to the system, which
//! already writes a far better one than we could.

use std::sync::atomic::AtomicBool;

/// Set for the whole life of the handler so a fault *inside* it (a corrupt
/// heap makes that entirely possible) does not recurse forever.
static HANDLING: AtomicBool = AtomicBool::new(false);

/// Installs the top-level fault handler. Call once, as early as possible after
/// logging is up.
pub use imp::install;


#[cfg(windows)]
mod imp {
    use super::HANDLING;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION, EXCEPTION_STACK_OVERFLOW,
        GENERIC_WRITE, HANDLE, HMODULE, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION,
        MiniDumpWithIndirectlyReferencedMemory, MiniDumpWriteDump, SetUnhandledExceptionFilter,
    };
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
    };
    use windows::core::PCWSTR;

    /// Installs the top-level exception filter.
    pub fn install() {
        unsafe {
            SetUnhandledExceptionFilter(Some(handler));
        }
    }

    unsafe extern "system" fn handler(info: *const EXCEPTION_POINTERS) -> i32 {
        if HANDLING.swap(true, Ordering::SeqCst) {
            return EXCEPTION_EXECUTE_HANDLER;
        }
        report(info);
        // Written after the report, so a failure to produce the dump can never cost
        // us the one line that actually names the culprit.
        let dump = write_minidump(info);

        log::error!("Pubsplash is terminating because of the fault above");
        if let Some(path) = dump {
            log::error!("Crash dump written to {}", path.display());
        }
        // Buffered on a background writer that will not get another chance to run.
        log::logger().flush();
        EXCEPTION_EXECUTE_HANDLER
    }

    /// Logs what happened and, crucially, in whose code.
    fn report(info: *const EXCEPTION_POINTERS) {
        let record = unsafe { info.as_ref() }.and_then(|i| unsafe { i.ExceptionRecord.as_ref() });
        let Some(record) = record else {
            log::error!("Unhandled exception with no exception record");
            return;
        };

        let code = record.ExceptionCode.0 as u32;
        let address = record.ExceptionAddress as usize;
        let (module, offset) = module_for(address);

        log::error!(
            "FATAL: unhandled exception {code:#010x} ({}) at {address:#018x} on thread {} \
             — in {module}+{offset:#x}",
            exception_name(code),
            unsafe { GetCurrentThreadId() },
        );
        if let Some(detail) = access_violation_detail(code, record.NumberParameters, unsafe {
            std::ptr::addr_of!(record.ExceptionInformation).read_unaligned()
        }) {
            log::error!("FATAL: {detail}");
        }
    }

    /// The friendly name for the codes we are realistically going to see.
    pub fn exception_name(code: u32) -> &'static str {
        match code {
            c if c == EXCEPTION_ACCESS_VIOLATION.0 as u32 => "access violation",
            c if c == EXCEPTION_STACK_OVERFLOW.0 as u32 => "stack overflow",
            c if c == EXCEPTION_ILLEGAL_INSTRUCTION.0 as u32 => "illegal instruction",
            0xC000_0374 => "heap corruption",
            0xC000_0409 => "stack buffer overrun",
            0xE06D_7363 => "C++ exception",
            _ => "unknown",
        }
    }

    /// An access violation carries what it was doing and to what address, which
    /// separates "called through a freed vtable" from "wrote past a buffer".
    pub fn access_violation_detail(code: u32, count: u32, params: [usize; 15]) -> Option<String> {
        if code != EXCEPTION_ACCESS_VIOLATION.0 as u32 || count < 2 {
            return None;
        }
        let what = match params[0] {
            0 => "read from",
            1 => "wrote to",
            8 => "executed",
            _ => "touched",
        };
        let target = params[1];
        let (module, offset) = module_for(target);
        Some(format!(
            "the faulting instruction {what} {target:#018x} ({module}+{offset:#x})"
        ))
    }

    /// Writes a minidump beside the log files. Best effort — a failure here is
    /// logged and otherwise ignored.
    fn write_minidump(info: *const EXCEPTION_POINTERS) -> Option<PathBuf> {
        let dir = crate::config::config_dir().join("crashes");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::error!("Could not create the crash dump directory: {e}");
            return None;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("pubsplash-{stamp}.dmp"));

        let wide: Vec<u16> = {
            use std::os::windows::ffi::OsStrExt;
            path.as_os_str().encode_wide().chain([0]).collect()
        };
        let file = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_READ,
                None,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        let file: HANDLE = match file {
            Ok(handle) if handle != INVALID_HANDLE_VALUE => handle,
            _ => {
                log::error!("Could not create {}", path.display());
                return None;
            }
        };

        let mut exception = MINIDUMP_EXCEPTION_INFORMATION {
            ThreadId: unsafe { GetCurrentThreadId() },
            ExceptionPointers: info as *mut EXCEPTION_POINTERS,
            ClientPointers: false.into(),
        };
        let result = unsafe {
            MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                file,
                MiniDumpWithIndirectlyReferencedMemory,
                Some(&mut exception as *const _),
                None,
                None,
            )
        };
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(file);
        }
        match result {
            Ok(()) => Some(path),
            Err(e) => {
                log::error!("Could not write the crash dump: {e}");
                None
            }
        }
    }

    pub fn module_for(address: usize) -> (String, usize) {
        if address == 0 {
            return ("<null>".to_string(), 0);
        }
        let mut module = HMODULE::default();
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(address as *const u16),
                &mut module,
            )
        };
        if found.is_err() || module.is_invalid() {
            // No mapped module owns it — the classic shape of a call through a
            // pointer into a DLL that has just been unloaded.
            return ("<no mapped module>".to_string(), address);
        }
        let mut buf = [0u16; 260];
        let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
        let name = if len == 0 {
            format!("{:#x}", module.0 as usize)
        } else {
            String::from_utf16_lossy(&buf[..len])
        };
        (name, address.wrapping_sub(module.0 as usize))
    }
}

/// A `sigaction` for each signal a faulting plugin can raise.
///
/// There is no macOS equivalent of `SetUnhandledExceptionFilter` — the nearest
/// thing is a Mach exception port, which is a whole server of its own — so this
/// is the ordinary POSIX route. It costs one property Windows gives free: a
/// signal handler runs on the faulting thread's stack, which for a stack
/// overflow is the one thing that has run out. `SIGSEGV` from an overflow is
/// therefore delivered on an alternate stack (`SA_ONSTACK`) that Rust's own
/// runtime has already installed for exactly this reason, and which our handler
/// inherits by asking for it.
///
/// No dump is written here. macOS already writes a full `.ips` crash report to
/// `~/Library/Logs/DiagnosticReports` for any process that dies this way, with
/// symbolicated frames for every loaded image — strictly better than the
/// minidump the Windows side has to produce itself. The log line points at it.
#[cfg(target_os = "macos")]
mod imp {
    use super::HANDLING;
    use std::sync::atomic::Ordering;

    /// The faults third-party audio code actually dies from. `SIGABRT` is in
    /// the list because a C++ library that throws through a `noexcept` boundary
    /// — or calls `std::terminate` any other way — lands there, and that is
    /// just as much "a plugin killed us" as a bad pointer is.
    const FATAL: [i32; 5] = [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGABRT,
    ];

    /// How many frames to name. Deep enough to cross the host shim and reach
    /// the plugin, short enough to stay a fixed-size array on a stack that may
    /// be nearly gone.
    const FRAMES: usize = 24;

    pub fn install() {
        for signal in FATAL {
            // SAFETY: `action` is fully initialized below and lives across the
            // call; `sigaction` copies it.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                // Through the fn-pointer type first: casting a fn *item*
                // straight to an integer is a lint, and rightly, because the
                // item is zero-sized and the cast reads as if it were not.
                let handler: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void) = handler;
                action.sa_sigaction = handler as usize;
                action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESETHAND;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }

    /// `SA_RESETHAND` restores the default disposition before this runs, so
    /// returning from here re-raises the signal against `SIG_DFL` and the
    /// process dies exactly as it would have — which is what lets the system
    /// write its own crash report on the way out. Nothing here calls `exit`.
    extern "C" fn handler(signal: i32, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
        if HANDLING.swap(true, Ordering::SeqCst) {
            return;
        }
        report(signal, info);
        // Buffered on a background writer that will not get another chance to
        // run once this returns and the signal is re-raised.
        log::logger().flush();
    }

    fn report(signal: i32, info: *mut libc::siginfo_t) {
        // SAFETY: the kernel hands us a valid `siginfo_t` for the signals we
        // asked for with `SA_SIGINFO`.
        let address = unsafe { info.as_ref() }.map_or(0, |i| i.si_addr as usize);
        let (module, offset) = module_for(address);
        log::error!(
            "FATAL: {} ({signal}) touching {address:#018x} — in {module}+{offset:#x}",
            signal_name(signal),
        );
        for (depth, frame) in frames().into_iter().enumerate() {
            let (module, offset) = module_for(frame);
            log::error!("FATAL:   #{depth:<2} {frame:#018x} {module}+{offset:#x}");
        }
        log::error!(
            "Pubsplash is terminating because of the fault above; \
             macOS will write a full crash report to ~/Library/Logs/DiagnosticReports"
        );
    }

    /// The return addresses on this thread's stack, deepest first.
    ///
    /// For a `SIGSEGV` the address the kernel reports is the one that was
    /// *accessed*, which for a call through a freed vtable is not in any module
    /// at all. The stack is what names the code that made the call, and is
    /// usually the more useful of the two.
    fn frames() -> Vec<usize> {
        let mut buf = [std::ptr::null_mut::<libc::c_void>(); FRAMES];
        // SAFETY: `backtrace` writes at most `FRAMES` entries into `buf`.
        let count = unsafe { libc::backtrace(buf.as_mut_ptr(), FRAMES as libc::c_int) };
        buf.iter()
            .take(count.max(0) as usize)
            .map(|frame| *frame as usize)
            // The top frames are this handler and the kernel trampoline, which
            // say nothing about the fault.
            .skip(2)
            .collect()
    }

    fn signal_name(signal: i32) -> &'static str {
        match signal {
            libc::SIGSEGV => "segmentation fault",
            libc::SIGBUS => "bus error",
            libc::SIGILL => "illegal instruction",
            libc::SIGFPE => "arithmetic fault",
            libc::SIGABRT => "abort",
            _ => "unknown fault",
        }
    }

    /// `dladdr` answers with the image containing an address and that image's
    /// load address, which is the same pair `GetModuleHandleExW` gives on
    /// Windows.
    pub fn module_for(address: usize) -> (String, usize) {
        if address == 0 {
            return ("<null>".to_string(), 0);
        }
        // SAFETY: `info` is fully written by `dladdr` when it answers non-zero,
        // and read only in that branch.
        unsafe {
            let mut info: libc::Dl_info = std::mem::zeroed();
            if libc::dladdr(address as *const libc::c_void, &mut info) == 0
                || info.dli_fname.is_null()
            {
                return ("<no mapped module>".to_string(), address);
            }
            let name = std::ffi::CStr::from_ptr(info.dli_fname)
                .to_string_lossy()
                .into_owned();
            (name, address.wrapping_sub(info.dli_fbase as usize))
        }
    }
}

#[cfg(test)]
mod tests {
    /// The module owning an address, and the offset into it. This is the whole
    /// point of the handler: it is the difference between "Pubsplash crashed"
    /// and "WaveShell1-VST3 12.7_x64.vst3 crashed".
    ///
    /// Both platforms answer `<null>` for a zero address and `<no mapped
    /// module>` for an address no image covers — the classic shape of a call
    /// through a pointer into a library that has just been unloaded — because
    /// that distinction is what the log reader needs and neither OS draws it
    /// for us. These tests hold whichever one is compiled in to that.
    use super::imp::module_for;

    /// The one fact the handler exists to report, asked of whichever
    /// implementation is compiled in.
    #[test]
    fn an_address_inside_our_own_image_resolves_to_this_executable() {
        let (module, offset) = module_for(module_for as *const () as usize);
        assert!(
            module.to_ascii_lowercase().contains("pubsplash"),
            "expected our own image, got {module}"
        );
        assert!(offset > 0, "an offset into the image, not the base");
    }

    #[test]
    fn an_unmapped_address_is_reported_as_unmapped() {
        let (module, _) = module_for(1);
        assert_eq!(module, "<no mapped module>");
        assert_eq!(module_for(0).0, "<null>");
    }

    /// Access-violation parameters are a Win32 shape; a signal carries no
    /// equivalent, so there is nothing to assert about one on macOS.
    #[cfg(windows)]
    #[test]
    fn an_access_violation_says_what_it_was_doing() {
        let mut params = [0usize; 15];
        params[0] = 1;
        params[1] = 0xdead_beef;
        use imp::access_violation_detail;
        use windows::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
        let detail =
            access_violation_detail(EXCEPTION_ACCESS_VIOLATION.0 as u32, 2, params).unwrap();
        assert!(detail.contains("wrote to"), "{detail}");
        assert!(detail.contains("deadbeef"), "{detail}");
        // Anything that is not an access violation carries no such parameters.
        assert!(access_violation_detail(0xC000_0374, 2, params).is_none());
    }
}
