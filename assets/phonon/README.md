# Phonon

Everything the Phonon speech engine (`src/tts/engines/phonon.rs`) needs, vendored
so that Pubsplash depends on nothing else being installed: no Python, no Phonon
SAPI or NVDA add-on, no download on first use. None of it is compiled into the
executable; it all ships as ordinary files beside it.

| In this folder | Beside the executable | What it is |
|---|---|---|
| `model.safetensors` | `phonon-model.safetensors` | Gradium's 40M multilingual Phonon model (English, French, German, Spanish, Portuguese), repackaged for the native runtime. Committed as `model.safetensors.001` and `.002`, which `build.rs` joins into this file (gitignored). |
| `config.json` | `phonon-config.json` | The model's configuration |
| `tokenizer.json` | `phonon-tokenizer.json` | The model's tokenizer |
| `voices/<locale>/<Name>.safetensors` | `voices\<locale>\<Name>.safetensors` | One voice each: speaker latents (`speaker_wavs`, `[1, 512, 125]` f32) |

The model is split because it is over GitHub's 100 MB file limit, and Git LFS
is not an option: GitHub refuses LFS uploads to a public fork. To replace it,
split the new file into parts under 100 MB with the same names.

The model, config and tokenizer are one matched set; replace them together.
They are renamed on the way out so that they read as Phonon's beside the exe.
What copies them there: `[package.metadata.packager] resources` in `Cargo.toml`
(the installer and the macOS bundle, where they land in `Contents/Resources`),
the portable ZIP step in `.github/workflows/release.yml`, and
`tools/deploy.ps1`. A development build finds them here in the checkout.

The runtime is the `ptts` crate from crates.io
([gradium-ai/xn-ptts](https://github.com/gradium-ai/xn-ptts), MIT OR Apache-2.0),
linked in-process. Synthesis runs on the CPU.

## Voices

The subfolder a voice is in is its language: it decides which entry of the
voice picker's language filter lists it, and how its text is normalized before
synthesis (the first two letters, `es_MX` → Spanish). `en` holds Gradium's
English character voices, which have no regional locale. A file put straight
into `voices` with no subfolder is listed under "Other languages", and its text
is passed to the model as written.

Shipped: the 102 voices of Gradium's catalogue, plus Oriol and Ricardo (`es_ES`).

**To add a voice**, drop its `.safetensors` file into the `voices` folder beside
`pubsplash.exe` — in the right language subfolder — and restart Pubsplash, since
the voice list is read at startup. The file name, without the extension, is the
voice's name and its stored id; if two files share a name, one is ignored and the
log says which. To add one to every build, put it here instead.

Updates replace the voices Pubsplash ships and leave any others in the folder
alone, the portable self-update included (`SHIPPED_FOLDER` in
`src/update/mod.rs` and `src/bin/updater_win.rs`).

Pubsplash does not create voices.

## Licences

- Model: CC BY-NC 4.0, see `LICENSE-model.txt`. Non-commercial use only.
- Voice catalogue: Apache-2.0, see `LICENSE-voices.txt`.
