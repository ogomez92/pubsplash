// Embeds Windows resources so Explorer, the taskbar, Alt+Tab, and wxWidgets all
// see the executable metadata they expect. This is separate from cargo-packager's
// `icons`, which only covers the installer and shortcuts.
fn main() {
    validate_default_sound_pack();

    join_phonon_model();

    embed_windows_resources();
}

/// Rebuilds `assets/phonon/model.safetensors` from the halves Git stores it in.
///
/// The model is 121 MB, over GitHub's 100 MB file limit, and Git LFS is not an
/// option: GitHub refuses LFS uploads to a public fork, which this repository
/// is. So it is committed as `model.safetensors.001` and `.002`, and joined here
/// into the path every consumer already reads — the packager's `resources`, the
/// workflow's portable ZIP step, `tools/deploy.ps1`, and a development build's
/// own lookup in `tts::engines::phonon`. The joined file is gitignored.
///
/// Rewritten only when its size disagrees with the parts', so an ordinary build
/// costs two `stat`s. Written to a temporary name and renamed, so an
/// interrupted build cannot leave a truncated model that looks complete.
fn join_phonon_model() {
    const DIR: &str = "assets/phonon";
    const OUTPUT: &str = "assets/phonon/model.safetensors";
    let parts: Vec<std::path::PathBuf> = (1..)
        .map(|n| std::path::PathBuf::from(format!("{OUTPUT}.{n:03}")))
        .take_while(|path| path.is_file())
        .collect();
    for part in &parts {
        println!("cargo:rerun-if-changed={}", part.display());
    }
    println!("cargo:rerun-if-changed={OUTPUT}");
    if parts.is_empty() {
        println!("cargo:warning=No Phonon model parts in {DIR}; the Phonon engine will report its model missing.");
        return;
    }
    let expected: u64 = parts
        .iter()
        .map(|part| std::fs::metadata(part).map(|m| m.len()).unwrap_or(0))
        .sum();
    if std::fs::metadata(OUTPUT).map(|m| m.len()).ok() == Some(expected) {
        return;
    }
    let temporary = format!("{OUTPUT}.joining");
    {
        let mut out = std::fs::File::create(&temporary).expect("creating the joined Phonon model");
        for part in &parts {
            let mut input = std::fs::File::open(part).expect("opening a Phonon model part");
            std::io::copy(&mut input, &mut out).expect("joining the Phonon model");
        }
    }
    std::fs::rename(&temporary, OUTPUT).expect("putting the joined Phonon model in place");
}

/// `winresource` is a Windows-only build dependency, so this cannot be a
/// runtime `if` on `CARGO_CFG_WINDOWS` -- the crate is not in the graph at all
/// on another platform and naming it would fail to compile. `cfg(windows)` in a
/// build script is the *host*, which is also what a `[target.'cfg(windows)']`
/// build-dependency section matches; the two agree because Windows builds are
/// native.
#[cfg(windows)]
fn embed_windows_resources() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon/pubsplash.ico");
    res.set_manifest(COMMON_CONTROLS_V6_MANIFEST);
    res.compile().expect("failed to embed Windows resources");
}

#[cfg(not(windows))]
fn embed_windows_resources() {}

#[cfg(windows)]
const COMMON_CONTROLS_V6_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="Pubsplash" type="win32"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;

fn validate_default_sound_pack() {
    const PATH: &str = "assets/sounds/default/default.pspack";
    println!("cargo:rerun-if-changed={PATH}");

    let bytes = std::fs::read(PATH).expect("default sound pack must exist");
    assert!(
        bytes.len() >= 42,
        "default sound pack is too short to be a Pubsplash sound pack"
    );
    assert!(
        bytes.starts_with(b"PSSP"),
        "default sound pack does not have the Pubsplash sound-pack header"
    );
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    assert_eq!(
        version, 1,
        "default sound pack uses unsupported format version {version}"
    );
}
