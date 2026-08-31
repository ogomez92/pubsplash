//! Minimal VST3 module inspection: load the module, get its IPluginFactory,
//! and list the audio module classes. Hand-rolled COM-style vtable calls —
//! no Steinberg SDK bindings. Compiled only into the `pubsplash-scan` helper
//! process (via `#[path]` in `src/bin/scan_helper.rs`).
//!
//! Almost all of this is portable: VST3's factory interface is the same
//! `FUnknown`-derived vtable on every platform, the `PClassInfo` layout is
//! fixed by the specification, and `extern "system"` is `extern "C"` everywhere
//! Pubsplash runs. The platform seam is only how the module is opened and a
//! symbol found — `LoadLibraryExW`/`GetProcAddress` against `dlopen`/`dlsym`.

use super::types::ScanOutput;
use std::ffi::{CStr, c_void};
use std::path::Path;

const K_RESULT_OK: i32 = 0;
const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

type GetFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;

#[repr(C)]
struct IPluginFactory {
    vtable: *const IPluginFactoryVtbl,
}

/// FUnknown's three methods followed by IPluginFactory's own.
#[repr(C)]
struct IPluginFactoryVtbl {
    query_interface:
        unsafe extern "system" fn(*mut IPluginFactory, *const c_void, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IPluginFactory) -> u32,
    release: unsafe extern "system" fn(*mut IPluginFactory) -> u32,
    get_factory_info: unsafe extern "system" fn(*mut IPluginFactory, *mut PFactoryInfo) -> i32,
    count_classes: unsafe extern "system" fn(*mut IPluginFactory) -> i32,
    get_class_info: unsafe extern "system" fn(*mut IPluginFactory, i32, *mut PClassInfo) -> i32,
    create_instance: *const c_void,
}

#[repr(C)]
struct PFactoryInfo {
    vendor: [u8; 64],
    url: [u8; 256],
    email: [u8; 128],
    flags: i32,
}

#[repr(C)]
struct PClassInfo {
    cid: [u8; 16],
    cardinality: i32,
    category: [u8; 32],
    name: [u8; 64],
}

fn fixed_string(buf: &[u8]) -> String {
    CStr::from_bytes_until_nul(buf)
        .map(|s| s.to_string_lossy().trim().to_string())
        .unwrap_or_else(|_| String::from_utf8_lossy(buf).trim().to_string())
}

fn cid_hex(cid: &[u8; 16]) -> String {
    cid.iter().map(|b| format!("{b:02X}")).collect()
}

pub fn scan(path: &Path) -> Result<ScanOutput, String> {
    let module = imp::open(path)?;
    unsafe {
        // The module's own initializer, if it has one. Windows calls it
        // `InitDll`; macOS calls it `bundleEntry` and hands it the bundle, which
        // the module keeps a reference to -- so the two are not the same
        // function under different names and `imp` supplies each its own way.
        imp::initialize(&module)?;

        let get_factory = module
            .symbol("GetPluginFactory")
            .ok_or("no GetPluginFactory export")?;
        let get_factory: GetFactoryFn = std::mem::transmute(get_factory);
        let factory = get_factory();
        if factory.is_null() {
            return Err("GetPluginFactory returned null".to_string());
        }
        let vtbl = &*(*factory).vtable;

        let mut factory_info: PFactoryInfo = std::mem::zeroed();
        let vendor = if (vtbl.get_factory_info)(factory, &mut factory_info) == K_RESULT_OK {
            fixed_string(&factory_info.vendor)
        } else {
            String::new()
        };

        let mut name = String::new();
        let mut class_ids = Vec::new();
        let count = (vtbl.count_classes)(factory);
        for index in 0..count {
            let mut class_info: PClassInfo = std::mem::zeroed();
            if (vtbl.get_class_info)(factory, index, &mut class_info) != K_RESULT_OK {
                continue;
            }
            if fixed_string(&class_info.category) != AUDIO_MODULE_CLASS {
                continue;
            }
            class_ids.push(cid_hex(&class_info.cid));
            if name.is_empty() {
                name = fixed_string(&class_info.name);
            }
        }
        (vtbl.release)(factory);
        // ExitDll is deliberately not called: some plugins misbehave when torn
        // down, and this process exits immediately anyway.

        if class_ids.is_empty() {
            return Err("no audio module classes".to_string());
        }
        if name.is_empty() {
            name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        Ok(ScanOutput {
            name,
            vendor,
            version: String::new(),
            unique_id: None,
            class_ids,
        })
    }
}

/// `LoadLibraryExW` with `LOAD_WITH_ALTERED_SEARCH_PATH`, so dependency DLLs
/// shipped next to the plugin resolve from the plugin's folder rather than from
/// the helper's.
#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
    };
    use windows::core::{PCSTR, PCWSTR};

    type InitDllFn = unsafe extern "system" fn() -> bool;

    pub struct Module(HMODULE);

    impl Module {
        pub fn symbol(&self, name: &str) -> Option<*const c_void> {
            // `GetProcAddress` wants a NUL-terminated ASCII name, and every
            // symbol this file asks for is a literal in the caller.
            let name = format!("{name}\0");
            unsafe { GetProcAddress(self.0, PCSTR(name.as_ptr())) }
                .map(|f| f as *const c_void)
        }
    }

    pub fn open(path: &Path) -> Result<Module, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH)
                .map(Module)
                .map_err(|e| format!("could not load module: {e}"))
        }
    }

    /// # Safety
    /// `module` must be a loaded VST3 module.
    pub unsafe fn initialize(module: &Module) -> Result<(), String> {
        let Some(init) = module.symbol("InitDll") else {
            return Ok(());
        };
        let init: InitDllFn = unsafe { std::mem::transmute(init) };
        if unsafe { init() } {
            Ok(())
        } else {
            Err("InitDll returned false".to_string())
        }
    }
}

/// `dlopen`/`dlsym`.
///
/// `RTLD_LOCAL` keeps the plugin's symbols out of the global namespace, so two
/// plugins exporting the same symbol cannot bind to each other's — the nearest
/// thing here to what `LOAD_WITH_ALTERED_SEARCH_PATH` protects against on
/// Windows. A bundle's own dependencies are found through its `LC_RPATH`, which
/// is the loader's business and needs nothing from us.
///
/// The module is deliberately never `dlclose`d: this process exits immediately
/// after printing, and unloading a plugin that has started a thread is the class
/// of crash the helper exists to keep away from Pubsplash.
#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CString, c_void};
    use std::path::Path;

    const RTLD_NOW: i32 = 0x2;
    const RTLD_LOCAL: i32 = 0x4;

    /// The macOS module entry point. Unlike Windows' `InitDll` it is handed the
    /// module's own `CFBundleRef`, which the plugin keeps in order to find its
    /// resources — so it cannot be called with no argument.
    type BundleEntryFn = unsafe extern "system" fn(*mut c_void) -> bool;

    pub struct Module(*mut c_void);

    impl Module {
        pub fn symbol(&self, name: &str) -> Option<*const c_void> {
            let name = CString::new(name).ok()?;
            // SAFETY: `self.0` is a live handle from `dlopen`.
            let sym = unsafe { libc::dlsym(self.0, name.as_ptr()) };
            (!sym.is_null()).then_some(sym as *const c_void)
        }
    }

    pub fn open(path: &Path) -> Result<Module, String> {
        let c_path =
            CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: `c_path` is a NUL-terminated path that outlives the call.
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
        if handle.is_null() {
            // SAFETY: `dlerror` returns a static message or null.
            let reason = unsafe {
                let msg = libc::dlerror();
                if msg.is_null() {
                    "unknown error".to_string()
                } else {
                    std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned()
                }
            };
            return Err(format!("could not load module: {reason}"));
        }
        Ok(Module(handle))
    }

    /// # Safety
    /// `module` must be a loaded VST3 module.
    ///
    /// The bundle pointer passed to `bundleEntry` is null. That is not a
    /// shortcut: this helper only reads the factory's class list, and a module
    /// that genuinely needs its bundle to do that will say so by returning
    /// false, which is reported as a scan failure like any other. Building a
    /// real `CFBundleRef` would mean linking CoreFoundation into the helper for
    /// the benefit of plugins that have not been observed to need it.
    pub unsafe fn initialize(module: &Module) -> Result<(), String> {
        let Some(entry) = module.symbol("bundleEntry") else {
            return Ok(());
        };
        let entry: BundleEntryFn = unsafe { std::mem::transmute(entry) };
        if unsafe { entry(std::ptr::null_mut()) } {
            Ok(())
        } else {
            Err("bundleEntry returned false".to_string())
        }
    }
}
