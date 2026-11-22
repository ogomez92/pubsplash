//! Phonon — Gradium's offline neural voices, synthesized in-process.
//!
//! Everything is vendored, so nothing has to be installed or downloaded: the
//! runtime is the `ptts` crate, linked in, and the rest ships as ordinary files
//! beside the executable, so it can be seen and changed in an install — the
//! model as `phonon-model.safetensors` with its `phonon-config.json` and
//! `phonon-tokenizer.json` (see [`model_file_path`]), and the **`voices`
//! folder** (see [`voices_dir`]), which a user adds a voice to by dropping a
//! `.safetensors` file in it. Nothing of Phonon's is compiled into the exe.
//!
//! It is not a network engine, but it runs on the `tts-net` worker all the same,
//! because that worker is simply "every engine that is a [`SpeechEngine`]" — the
//! only engine that is not is the platform voice, which owns a thread of its
//! own. Synthesis is CPU-bound and several times faster than real time, and it
//! streams: the first chunk reaches the mixer in about a tenth of a second.
//!
//! # One model, loaded once
//!
//! The model is loaded on the first utterance and kept for the life of the
//! process in [`RUNTIME`], because [`SpeechEngine`]s are rebuilt for every
//! request and a load per message would be wasted work. A user who never picks
//! Phonon never pays for it. The lock is held for a whole utterance: there is
//! one model and one generation at a time, which is also all the worker wants.
//! The voice preview, on its own thread, simply waits its turn.
//!
//! # Languages
//!
//! A voice's language is the subfolder it lives in (`voices/es_ES/Oriol`), which
//! is what the source dialog's language filter lists. It also decides how the
//! text is normalized before synthesis — numbers, symbols — and that is per
//! request, not per model: the model is built with normalization off and
//! [`Normalize::apply`] runs here, so one loaded model speaks every language.
//! A file put straight into `voices/` has no language: it is listed under
//! "Other languages" and its text is passed through as written.
//!
//! # Rate
//!
//! Native, through the model's `duration_delta` condition (negative is faster),
//! so a faster voice is generated faster rather than time-stretched. Pitch has
//! no equivalent and is not offered.

use crate::config::TtsEngineSettings;
use crate::tts::engine::{SpeechEngine, SynthRequest, TtsError, Voice};
use ptts::preprocess::{Lang, Normalize};
use ptts::synth::{DeviceKind, SpeechOptions, Synth};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The model's three files: what each is called beside the executable, and
/// what it is called in the checkout's `assets/phonon/`. They are one matched
/// set — a config or tokenizer from another checkpoint loads and then speaks
/// nonsense — so all three ship as files, side by side, and none is compiled in.
struct ModelFile {
    shipped: &'static str,
    in_checkout: &'static str,
}
const MODEL_FILE: ModelFile = ModelFile {
    shipped: "phonon-model.safetensors",
    in_checkout: "model.safetensors",
};
const CONFIG_FILE: ModelFile = ModelFile {
    shipped: "phonon-config.json",
    in_checkout: "config.json",
};
const TOKENIZER_FILE: ModelFile = ModelFile {
    shipped: "phonon-tokenizer.json",
    in_checkout: "tokenizer.json",
};

/// The folder beside the executable that holds the voices, one subfolder per
/// language: `voices/<locale>/<name>.safetensors`.
const VOICES_DIR: &str = "voices";

/// The voice a source that names none speaks with, when it has no language
/// filter either. Used to pick the first voice of the filtered language
/// otherwise.
const DEFAULT_VOICE: &str = "Elodie-Rose";

/// The model's CPU pool width. More is slower, not faster: on a 32-thread
/// machine four threads took 0.44 s for a sentence that every core took 0.70 s
/// over, and the spare cores belong to the mixer and the encoder.
const MAX_THREADS: usize = 4;

/// The locales the shipped voices come in, in the order the language filter
/// lists them. A folder with any other name is listed after these, under its
/// own name, and loose files with no folder come last.
pub const LOCALES: &[&str] = &[
    "en_US", "en_GB", "en", "es_ES", "es_MX", "fr_FR", "de_DE", "pt_BR",
];

/// The model, once loaded, and the voices it has been given so far.
static RUNTIME: Mutex<Option<Loaded>> = Mutex::new(None);

struct Loaded {
    synth: Synth,
    /// Voice name → the file it was registered from, so a voice whose file is
    /// replaced by one of the same name is registered again rather than
    /// served from the copy loaded earlier.
    registered: HashMap<String, PathBuf>,
    /// The rate the model's conditions were last set for.
    rate: i32,
}

/// Sizes the model's CPU pool. Called from `main` before any thread exists,
/// because the only way to set it also writes `RAYON_NUM_THREADS` into the
/// process environment.
pub fn configure_threads() {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    xn::set_num_threads(cpus.clamp(1, MAX_THREADS));
}

/// What a language filter entry is called. The empty locale is the voices put
/// straight into the `voices` folder, with no language subfolder.
pub fn language_label(locale: &str) -> String {
    match locale {
        "en_US" => crate::t!("English (United States)"),
        "en_GB" => crate::t!("English (United Kingdom)"),
        "en" => crate::t!("English character voices"),
        "es_ES" => crate::t!("Spanish (Spain)"),
        "es_MX" => crate::t!("Spanish (Mexico)"),
        "fr_FR" => crate::t!("French"),
        "de_DE" => crate::t!("German"),
        "pt_BR" => crate::t!("Portuguese (Brazil)"),
        "" => crate::t!("Other languages"),
        other => other.to_string(),
    }
}

/// Every locale that has at least one voice, in filter order.
pub fn languages() -> Vec<String> {
    let mut present: Vec<String> = Vec::new();
    for voice in installed_voices() {
        if !present.contains(&voice.locale) {
            present.push(voice.locale);
        }
    }
    present.sort_by_key(|locale| locale_order(locale));
    present
}

/// Where a locale sits in the language filter: the shipped ones in [`LOCALES`]
/// order, then any other folder by name, then the loose voices.
fn locale_order(locale: &str) -> (usize, String) {
    let rank = if locale.is_empty() {
        LOCALES.len() + 1
    } else {
        LOCALES
            .iter()
            .position(|known| *known == locale)
            .unwrap_or(LOCALES.len())
    };
    (rank, locale.to_string())
}

/// How text in `locale` is normalized: by its language, the part before the
/// underscore. A language the normalizer does not know — or no language at
/// all — is left as written, which is what `ptts` itself recommends over
/// guessing.
fn normalization(locale: &str) -> Normalize {
    let language = locale.split(['_', '-']).next().unwrap_or("");
    match language.parse::<Lang>() {
        Ok(lang) => Normalize::for_lang(lang),
        Err(_) => Normalize::OFF,
    }
}

/// `rate` (-10..=10) as the model's `duration_delta`, which runs the other way:
/// negative is faster. Gradium's guidance is that small values work best and
/// ±0.3 is a comfortable range, so the slider's ends land there.
fn duration_delta(rate: i32) -> f32 {
    -(rate.clamp(-10, 10) as f32) * 0.03
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstalledVoice {
    name: String,
    /// The subfolder it is in; empty for a file straight in `voices/`.
    locale: String,
    path: PathBuf,
}

/// The voices in the `voices` folder, sorted by name.
fn installed_voices() -> Vec<InstalledVoice> {
    voices_dir()
        .map(|dir| scan_voice_dir(&dir))
        .unwrap_or_default()
}

/// The first of [`voices_candidates`] that exists: beside the
/// executable in an installed or portable copy, `Contents/Resources` in a macOS
/// bundle, the checkout's `assets/phonon/voices` in a development build.
fn voices_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    voices_candidates(exe_dir)
        .into_iter()
        .find(|dir| dir.is_dir())
}

fn voices_candidates(exe_dir: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = install_dirs(exe_dir)
        .into_iter()
        .map(|dir| dir.join(VOICES_DIR))
        .collect();
    candidates.extend(checkout_dirs(exe_dir).map(|dir| dir.join(VOICES_DIR)));
    candidates
}

/// Where shipped files sit: beside the executable, and in `Contents/Resources`
/// in a macOS bundle (a sibling of the executable's folder).
fn install_dirs(exe_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![exe_dir.to_path_buf()];
    if cfg!(target_os = "macos")
        && let Some(contents) = exe_dir.parent()
    {
        dirs.push(contents.join("Resources"));
    }
    dirs
}

/// `assets/phonon` in a checkout the executable was built in. `target/debug`
/// is two levels under the checkout, but a test binary runs from as deep as
/// `target/debug/build/pubsplash/<hash>/out`.
fn checkout_dirs(exe_dir: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    exe_dir
        .ancestors()
        .take(8)
        .map(|dir| dir.join("assets").join("phonon"))
}

/// Every `.safetensors` under `root`: in a language subfolder, which is its
/// locale, or loose in `root` itself, which has none. Sorted by name; when two
/// files share a name (compared case-insensitively, since a name is a voice's
/// stored id) the first by locale wins and the other is logged and skipped.
fn scan_voice_dir(root: &Path) -> Vec<InstalledVoice> {
    let mut voices = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return voices;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let locale = entry.file_name().to_string_lossy().into_owned();
            if let Ok(files) = std::fs::read_dir(&path) {
                for file in files.flatten() {
                    if let Some(voice) = voice_file(file.path(), &locale) {
                        voices.push(voice);
                    }
                }
            }
        } else if let Some(voice) = voice_file(path, "") {
            voices.push(voice);
        }
    }
    voices.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.locale.cmp(&b.locale))
    });
    voices.dedup_by(|later, kept| {
        let duplicate = later.name.eq_ignore_ascii_case(&kept.name);
        if duplicate {
            log::warn!(
                "Two Phonon voices are called {}; using {} and ignoring {}",
                kept.name,
                kept.path.display(),
                later.path.display()
            );
        }
        duplicate
    });
    voices
}

fn voice_file(path: PathBuf, locale: &str) -> Option<InstalledVoice> {
    let is_voice = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("safetensors"));
    if !is_voice || !path.is_file() {
        return None;
    }
    let name = path.file_stem()?.to_str()?.to_string();
    Some(InstalledVoice {
        name,
        locale: locale.to_string(),
        path,
    })
}

/// The list of shipped voices the portable ZIP carries at its top level, one
/// `<locale>/<name>.safetensors` per line. It exists for one job, see
/// [`adopt_flattened_voices`].
const VOICE_MANIFEST: &str = "phonon-voices.txt";

/// Puts back voices that an older portable copy's self-update unpacked flat.
///
/// Before 2.4.0 the portable updater kept only file names, so a copy updating
/// itself from 2.3.0 — with 2.3.0's own unpacking code and helper — lands every
/// voice loose beside the executable, and the language folder each one belongs
/// in is gone with the path. [`VOICE_MANIFEST`] is a top-level file and so
/// survives that intact; this moves each loose file it names into its folder.
/// Only files the manifest names are ever moved, so nothing of the user's is
/// touched, and when there is nothing loose (every install but that one) it is
/// a single failed open. Called from `main` once logging is up.
pub fn adopt_flattened_voices() {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let moved = adopt_flattened_voices_in(&exe_dir);
    if moved > 0 {
        log::info!(
            "Moved {moved} Phonon voice(s) left loose by an older update into {}",
            exe_dir.join(VOICES_DIR).display()
        );
    }
}

fn adopt_flattened_voices_in(dir: &Path) -> usize {
    let Ok(manifest) = std::fs::read_to_string(dir.join(VOICE_MANIFEST)) else {
        return 0;
    };
    let mut moved = 0;
    for line in manifest.lines().map(str::trim).filter(|line| !line.is_empty()) {
        // Only `<locale>/<file>` or `<file>`, nothing that climbs or roots.
        let parts: Vec<&str> = line.split(['/', '\\']).collect();
        let valid = matches!(parts.len(), 1 | 2)
            && parts
                .iter()
                .all(|part| !part.is_empty() && *part != "." && *part != ".." && !part.contains(':'));
        if !valid {
            continue;
        }
        let file_name = parts[parts.len() - 1];
        let loose = dir.join(file_name);
        if !loose.is_file() {
            continue;
        }
        let mut destination = dir.join(VOICES_DIR);
        for part in &parts {
            destination.push(part);
        }
        let result = destination
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                // The loose copy is the one the update just delivered.
                if destination.exists() {
                    std::fs::remove_file(&destination)?;
                }
                std::fs::rename(&loose, &destination)
            });
        match result {
            Ok(()) => moved += 1,
            Err(error) => log::warn!(
                "Could not move {} into {}: {error}",
                loose.display(),
                destination.display()
            ),
        }
    }
    moved
}

/// The voice a request will speak with: the one it names, else the first voice
/// of its language filter, else [`DEFAULT_VOICE`], else whatever is first.
fn resolve_voice<'a>(
    voices: &'a [InstalledVoice],
    request: &SynthRequest,
) -> Option<&'a InstalledVoice> {
    let wanted = request.voice.trim();
    if !wanted.is_empty() {
        return voices
            .iter()
            .find(|voice| voice.name.eq_ignore_ascii_case(wanted));
    }
    let language = match &request.provider_settings {
        Some(TtsEngineSettings::Phonon(settings)) => Some(settings.language.trim()),
        _ => None,
    };
    if let Some(language) = language.filter(|language| !language.is_empty())
        && let Some(voice) = voices.iter().find(|voice| voice.locale == language)
    {
        return Some(voice);
    }
    voices
        .iter()
        .find(|voice| voice.name == DEFAULT_VOICE)
        .or_else(|| voices.first())
}

/// Where one of the model's files is: the same places as [`voices_dir`], under
/// its shipped name beside the executable and its own name in a checkout.
fn model_file_path(file: &ModelFile) -> Result<PathBuf, TtsError> {
    let exe = std::env::current_exe().map_err(|e| TtsError::Other(e.to_string()))?;
    let exe_dir = exe.parent().unwrap_or(Path::new("."));
    model_candidates(exe_dir, file)
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            TtsError::NotConfigured(crate::t!(
                "The Phonon voice model file {file} is missing from the Pubsplash folder. Reinstall Pubsplash to restore it.",
                file = file.shipped
            ))
        })
}

fn model_candidates(exe_dir: &Path, file: &ModelFile) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = install_dirs(exe_dir)
        .into_iter()
        .map(|dir| dir.join(file.shipped))
        .collect();
    candidates.extend(checkout_dirs(exe_dir).map(|dir| dir.join(file.in_checkout)));
    candidates
}

fn load() -> Result<Loaded, TtsError> {
    let path = model_file_path(&MODEL_FILE)?;
    // A file this small is not the model — a failed join in a checkout, say —
    // and would otherwise fail inside the loader with a message about
    // safetensors that says nothing useful.
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size < 1024 * 1024 {
        return Err(TtsError::NotConfigured(crate::t!(
            "The Phonon voice model at {path} is incomplete. Reinstall Pubsplash to restore it.",
            path = path.display()
        )));
    }
    let started = std::time::Instant::now();
    let failed = |e: &dyn std::fmt::Display| {
        TtsError::Other(crate::t!(
            "The Phonon voice model could not be loaded: {error}",
            error = e
        ))
    };
    let config = ptts::checkpoint::read_config(model_file_path(&CONFIG_FILE)?)
        .map_err(|e| failed(&e))?;
    let tokenizer = ptts::tok::Tok::open(&model_file_path(&TOKENIZER_FILE)?)
        .map_err(|e| failed(&e))?;
    let synth = Synth::builder(config, &path, Normalize::OFF)
        .tokenizer(Box::new(tokenizer))
        .device(DeviceKind::Cpu)
        .build()
        .map_err(|e| failed(&e))?;
    log::info!(
        "Loaded the Phonon model from {} in {:?} ({} threads)",
        path.display(),
        started.elapsed(),
        xn::get_num_threads()
    );
    Ok(Loaded {
        synth,
        registered: HashMap::new(),
        // Forces the first request to set its conditions explicitly.
        rate: i32::MIN,
    })
}

impl Loaded {
    fn ensure_voice(&mut self, voice: &InstalledVoice) -> Result<(), TtsError> {
        if self.registered.get(&voice.name) == Some(&voice.path) {
            return Ok(());
        }
        self.synth
            .add_voice_file(&voice.name, &voice.path)
            .map_err(|error| {
                TtsError::Other(crate::t!(
                    "The Phonon voice {voice} could not be loaded: {error}",
                    voice = voice.name,
                    error = error
                ))
            })?;
        self.registered
            .insert(voice.name.clone(), voice.path.clone());
        Ok(())
    }

    fn ensure_rate(&mut self, rate: i32) -> Result<(), TtsError> {
        if self.rate == rate {
            return Ok(());
        }
        let mut conditions = HashMap::new();
        conditions.insert(
            "duration_delta".to_string(),
            format!("{:.3}", duration_delta(rate)),
        );
        self.synth
            .set_conditions(conditions)
            .map_err(|e| TtsError::Other(e.to_string()))?;
        self.rate = rate;
        Ok(())
    }
}

pub struct Phonon;

impl Phonon {
    pub fn new() -> Self {
        Self
    }
}

impl SpeechEngine for Phonon {
    fn id(&self) -> &'static str {
        super::PHONON
    }

    fn display_name(&self) -> &'static str {
        "Phonon"
    }

    fn synth(&self, request: &SynthRequest) -> Result<Vec<f32>, TtsError> {
        let mut samples = Vec::new();
        self.synth_to(request, &mut |chunk| samples.extend_from_slice(chunk))?;
        Ok(samples)
    }

    fn synth_to(
        &self,
        request: &SynthRequest,
        sink: &mut dyn FnMut(&[f32]),
    ) -> Result<(), TtsError> {
        let voices = installed_voices();
        if voices.is_empty() {
            return Err(TtsError::NotConfigured(crate::t!(
                "No Phonon voices were found. They belong in the voices folder beside Pubsplash."
            )));
        }
        let voice = resolve_voice(&voices, request).ok_or_else(|| {
            TtsError::NotConfigured(crate::t!(
                "The Phonon voice {voice} is not installed.",
                voice = request.voice.trim()
            ))
        })?;
        let text = normalization(&voice.locale)
            .apply(request.text.trim())
            .into_owned();
        if text.trim().is_empty() {
            return Ok(());
        }

        let mut runtime = RUNTIME.lock().unwrap_or_else(|e| e.into_inner());
        if runtime.is_none() {
            *runtime = Some(load()?);
        }
        let loaded = runtime.as_mut().expect("loaded just above");
        loaded.ensure_voice(voice)?;
        loaded.ensure_rate(request.rate)?;

        let gain = request.volume_percent() as f32 / 100.0;
        let mut resampler = crate::audio::convert::StereoStream::new(loaded.synth.sample_rate());
        let stream = loaded
            .synth
            .stream_with(&text, &SpeechOptions::default().voice(voice.name.clone()))
            .map_err(|e| TtsError::Other(e.to_string()))?;
        let mut stereo = Vec::new();
        for chunk in stream {
            let chunk = chunk.map_err(|e| TtsError::Other(e.to_string()))?;
            stereo.clear();
            stereo.extend(chunk.iter().flat_map(|sample| {
                let sample = sample * gain;
                [sample, sample]
            }));
            let ready = resampler.push(&stereo);
            if !ready.is_empty() {
                sink(&ready);
            }
        }
        let tail = resampler.finish();
        if !tail.is_empty() {
            sink(&tail);
        }
        Ok(())
    }

    fn voices(&self) -> Result<Vec<Voice>, TtsError> {
        Ok(installed_voices()
            .into_iter()
            .map(|voice| Voice {
                label: crate::t!(
                    "{voice}, {language}",
                    voice = voice.name,
                    language = language_label(&voice.locale)
                ),
                id: voice.name,
                styles: Vec::new(),
                roles: Vec::new(),
                // The filter reads this: the language picker is this engine's
                // "model", see `ui::scenes::TtsProviderControls::model_value`.
                // A loose voice still carries its (empty) locale, so it is
                // listed under "Other languages" rather than under every one.
                supported_engines: vec![voice.locale.clone()],
                language_code: voice.locale,
            })
            .collect())
    }

    fn usage_voice(&self, request: &SynthRequest) -> Option<String> {
        resolve_voice(&installed_voices(), request).map(|voice| voice.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(voice: &str, language: &str) -> SynthRequest {
        SynthRequest {
            text: "Hola".into(),
            voice: voice.into(),
            rate: 0,
            volume: 100,
            pitch: 0,
            provider_settings: Some(TtsEngineSettings::Phonon(
                crate::config::PhononTtsSettings {
                    language: language.into(),
                },
            )),
        }
    }

    fn voice(name: &str, locale: &str) -> InstalledVoice {
        InstalledVoice {
            name: name.into(),
            locale: locale.into(),
            path: PathBuf::from(format!("{locale}/{name}.safetensors")),
        }
    }

    fn shipped_voices() -> Vec<InstalledVoice> {
        scan_voice_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets")
                .join("phonon")
                .join(VOICES_DIR),
        )
    }

    /// The tensor a voice file holds, read from its safetensors header: eight
    /// bytes of length, then that much JSON.
    fn speaker_wavs(path: &Path) -> serde_json::Value {
        let bytes = std::fs::read(path).unwrap();
        let length = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
        let header: serde_json::Value = serde_json::from_slice(&bytes[8..8 + length]).unwrap();
        header["speaker_wavs"].clone()
    }

    /// The catalogue's 102 voices plus Oriol and Ricardo. Each must hold the
    /// latents the model is conditioned on — a file that does not fails only
    /// when someone picks it.
    #[test]
    fn the_shipped_voices_are_all_there_and_well_formed() {
        let voices = shipped_voices();
        assert_eq!(voices.len(), 104, "102 catalogue voices plus Oriol and Ricardo");
        for voice in &voices {
            let tensor = speaker_wavs(&voice.path);
            assert_eq!(tensor["dtype"], "F32", "{}", voice.name);
            assert_eq!(tensor["shape"].as_array().map(Vec::len), Some(3), "{}", voice.name);
            assert!(LOCALES.contains(&voice.locale.as_str()), "{} is in {}", voice.name, voice.locale);
        }
        for name in ["Oriol", "Ricardo"] {
            assert!(
                voices.iter().any(|v| v.name == name && v.locale == "es_ES"),
                "{name} should be a Spanish (Spain) voice"
            );
        }
        assert!(voices.iter().any(|v| v.name == DEFAULT_VOICE));
    }

    #[test]
    fn every_shipped_locale_has_a_language_filter_entry() {
        let voices = shipped_voices();
        for locale in LOCALES {
            assert!(voices.iter().any(|v| v.locale == *locale), "nothing in {locale}");
            assert_ne!(language_label(locale), *locale, "{locale} has no label");
        }
    }

    #[test]
    fn voices_resolve_by_name_then_language_then_default() {
        let voices = vec![
            voice("Alba", "es_ES"),
            voice("Elodie-Rose", "en_GB"),
            voice("Lalo", "es_MX"),
        ];
        assert_eq!(resolve_voice(&voices, &request("lalo", "")).unwrap().name, "Lalo");
        assert_eq!(resolve_voice(&voices, &request("", "es_MX")).unwrap().name, "Lalo");
        assert_eq!(resolve_voice(&voices, &request("", "")).unwrap().name, "Elodie-Rose");
        // A named voice that has gone is an error, not a quiet substitute.
        assert!(resolve_voice(&voices, &request("Nobody", "es_ES")).is_none());
    }

    #[test]
    fn the_voices_folder_takes_subfolders_as_languages_and_loose_files_as_none() {
        let root = std::env::temp_dir().join(format!("pubsplash-phonon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("pt_BR")).unwrap();
        std::fs::create_dir_all(root.join("es_ES")).unwrap();
        std::fs::write(root.join("pt_BR").join("Mine.safetensors"), b"x").unwrap();
        std::fs::write(root.join("pt_BR").join("notes.txt"), b"x").unwrap();
        std::fs::write(root.join("Loose.safetensors"), b"x").unwrap();
        // Same name, other folder: one of them is ignored, never both listed.
        std::fs::write(root.join("es_ES").join("mine.safetensors"), b"x").unwrap();
        let found = scan_voice_dir(&root);
        let _ = std::fs::remove_dir_all(&root);
        let summary: Vec<(&str, &str)> = found
            .iter()
            .map(|v| (v.name.as_str(), v.locale.as_str()))
            .collect();
        assert_eq!(summary, vec![("Loose", ""), ("mine", "es_ES")]);
    }

    /// What a 2.3.0 portable copy looks like after updating itself to this
    /// version: every voice loose beside the exe, the manifest beside them.
    #[test]
    fn voices_flattened_by_an_old_update_are_put_back_in_their_folders() {
        let root = std::env::temp_dir().join(format!("pubsplash-flat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("voices").join("es_ES")).unwrap();
        std::fs::write(
            root.join(VOICE_MANIFEST),
            "es_ES/Oriol.safetensors\nen/Grub the Ogre.safetensors\n../evil.safetensors\nes_ES/Gone.safetensors\n",
        )
        .unwrap();
        std::fs::write(root.join("Oriol.safetensors"), b"new oriol").unwrap();
        std::fs::write(root.join("Grub the Ogre.safetensors"), b"grub").unwrap();
        std::fs::write(root.join("evil.safetensors"), b"x").unwrap();
        std::fs::write(root.join("Mine.safetensors"), b"not in the manifest").unwrap();
        // An older copy of a shipped voice already in place is replaced.
        std::fs::write(root.join("voices").join("es_ES").join("Oriol.safetensors"), b"old").unwrap();

        let moved = adopt_flattened_voices_in(&root);

        let read = |path: PathBuf| std::fs::read(path).unwrap();
        assert_eq!(moved, 2);
        assert_eq!(read(root.join("voices").join("es_ES").join("Oriol.safetensors")), b"new oriol");
        assert_eq!(read(root.join("voices").join("en").join("Grub the Ogre.safetensors")), b"grub");
        assert!(!root.join("Oriol.safetensors").exists());
        // A line that climbs out is ignored, and so is anything not listed.
        assert!(root.join("evil.safetensors").exists());
        assert!(root.join("Mine.safetensors").exists());
        // Running again finds nothing to do.
        assert_eq!(adopt_flattened_voices_in(&root), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loose_voices_are_listed_last_as_other_languages() {
        assert_ne!(language_label(""), "");
        let mut present = vec!["es_ES", "", "en_US", "it_IT"];
        present.sort_by_key(|locale| locale_order(locale));
        assert_eq!(present, vec!["en_US", "es_ES", "it_IT", ""]);
    }

    #[test]
    fn text_is_normalized_for_the_voice_language() {
        assert_eq!(normalization("es_MX").as_str(), Normalize::for_lang(Lang::Es).as_str());
        assert_eq!(normalization("en").as_str(), Normalize::for_lang(Lang::En).as_str());
        assert_eq!(normalization("xx_YY").as_str(), Normalize::OFF.as_str());
        assert_eq!(normalization("").as_str(), Normalize::OFF.as_str());
    }

    #[test]
    fn rate_runs_the_condition_the_other_way_within_gradiums_range() {
        assert_eq!(duration_delta(0), 0.0);
        assert!(duration_delta(10) < 0.0, "faster is a negative delta");
        assert!((duration_delta(10) + 0.3).abs() < 1e-6);
        assert!((duration_delta(-10) - 0.3).abs() < 1e-6);
        assert_eq!(duration_delta(99), duration_delta(10));
    }

    #[test]
    fn the_model_and_voices_are_looked_for_beside_the_exe_and_in_a_checkout() {
        let exe_dir = Path::new("repo").join("target").join("debug");
        let checkout = Path::new("repo").join("assets").join("phonon");
        let models = model_candidates(&exe_dir, &MODEL_FILE);
        assert_eq!(models[0], exe_dir.join("phonon-model.safetensors"));
        assert!(models.contains(&checkout.join("model.safetensors")));
        let tokenizers = model_candidates(&exe_dir, &TOKENIZER_FILE);
        assert_eq!(tokenizers[0], exe_dir.join("phonon-tokenizer.json"));
        assert!(tokenizers.contains(&checkout.join("tokenizer.json")));
        let voices = voices_candidates(&exe_dir);
        assert_eq!(voices[0], exe_dir.join(VOICES_DIR));
        assert!(voices.contains(&checkout.join(VOICES_DIR)));
    }

    /// Loads the real model and speaks a sentence with Oriol. Ignored because it
    /// loads the 121 MB model, which the fast suite has no need to.
    #[test]
    #[ignore]
    fn phonon_synthesizes_with_a_shipped_voice() {
        let mut request = request("Oriol", "es_ES");
        request.text = "Hola, esto es una prueba de Phonon en Pubsplash.".into();
        let samples = Phonon::new().synth(&request).expect("synthesis");
        let seconds = samples.len() as f32
            / (crate::audio::convert::ENGINE_SAMPLE_RATE as usize
                * crate::audio::convert::ENGINE_CHANNELS) as f32;
        assert!(seconds > 1.0, "only {seconds} s of audio");
        assert!(samples.iter().any(|s| s.abs() > 0.05), "silent");
    }
}
