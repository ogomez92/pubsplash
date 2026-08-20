# Pubsplash

Pubsplash is a Windows app for accessible live audio streaming. It sends a mix to Audiopub or a direct Icecast server and works well with screen readers such as NVDA and JAWS.

You can combine microphones, desktop audio, application audio, text-to-speech, and sound cues; adjust the mix; add VST effects; read and send Audiopub chat; and record an MP3 locally.

## Before you begin

You need:

- Windows 10 or Windows 11
- An Audiopub account trusted to stream, or Icecast source credentials
- An audio device if you plan to use a microphone

## Install

Download the newest release from the [GitHub releases page](https://github.com/ironcross32/pubsplash/releases).

These two links always point at the newest release, so they never go stale:

- [**Installer**](https://github.com/ironcross32/pubsplash/releases/latest/download/pubsplash-setup.exe)
- [**Portable ZIP**](https://github.com/ironcross32/pubsplash/releases/latest/download/pubsplash-portable.zip)

The two keep their data in different places. The installed copy uses `%LOCALAPPDATA%\pubsplash`. The portable copy uses a `user_data` folder inside the folder you unzipped. This allows Pubsplash and your user data to travel with you. Updates leave `user_data` alone.

One thing does not travel with a portable copy: saved passwords and API keys are encrypted for the Windows account that entered them, so on a different machine or a different user account they read as blank and have to be entered again. Everything else will still work.

Both kinds keep themselves up to date — see [Automatic updates](#automatic-updates). Every release is also on the [releases page](https://github.com/ironcross32/pubsplash/releases) under its version number, along with debug symbols.

## Getting started

### Connecting to a service

1. Open **File > Setup streaming services**.
2. Select the built-in **Audiopub** service, or choose **Add** for a self-hosted Audiopub instance or Icecast.
3. Enter the requested details and choose **Connect**.

For Audiopub, you need the site address, your email, and your password. The Icecast server and port are filled in for you from the site address and only need changing if the instance publishes somewhere other than the usual `live.` host on port 8000.

For Icecast, you normally need the server, port, mount point, username, and source password. The username defaults to source; the mount point may be `/` for the server root. Icecast does not provide Audiopub chat, listener counts, archiving, or an Audiopub stream page.

### 2. Add audio

Open **Scenes and Sources**. A scene is a saved collection of sources; the default scene is ready to use. Select it, choose **Add source**, and select from one of the following:

- **Microphone** - an input device.
- **Desktop Audio** - system audio. Pubsplash excludes its own audio, preventing text-to-speech and sound cues from echoing into the stream unless you want them to (see below). Press **Edit** to capture a single playback device instead of all of them; see [Choosing devices](#choosing-devices).
- **Application** - one program, such as a browser, game, or music player. You can select a running program or type a name for one that will open later.
- **Text-to-Speech** - reads incoming Audiopub chat aloud.
- **Sound Events** - plays cues for listener and chat activity.
- **Media Player** - a folder of your own music, played into the mix. See [The media player](#the-media-player).

Each source appears as a strip in the **Home** mixer. Use its volume and mute controls to adjust it. Volume boost and monitoring are available in the context menu. Pubsplash should recover a temporarily unavailable source in most cases. While it's attempting to reconnect, the volume slider on its channel strip will reflect this.

### 3. Set stream information

Choose **File > Set stream info** and enter a title and description. You can also choose the audio quality, archiving options, and Mastodon options (See below). The title, description, archive choice, and recording choice are per-stream settings; the bitrate is remembered between sessions.

### 4. Go live

On **Home**, choose **Start streaming**. If stream information is missing, Pubsplash opens that dialog first. Choose **Stop streaming** when finished.

The stream overview reports status, duration, listeners, listener peak, and connection problems. It does not report a healthy stream until audio is actually being sent.

## Record without streaming

[press] **Start recording** on Home to save the current mix as an MP3 without connecting to a server. Once recording is underway, the button changes its state to **Stop recording**, hit that to finish.

Streaming and standalone recording cannot run at the same time. Recordings are named recording_date_time.mp3 and saved in the folder configured on **File > Preferences > Archiving**. The default is your Music library.

## The main concepts

- **Scenes** let you prepare different source combinations and switch between them.
- **Sources** produce audio. Their names describe what they capture, making several microphones or applications easier to distinguish.
- **Buses** are shared mixing points. Send multiple sources to a bus when they should share volume or effects.
- **Effects** are VST2 or VST3 plugins on a bus or the master output. Effects run from top to bottom and can be bypassed while live.
- **FX chains** can be saved in the library or exported as .pubfx files.

To route a source, select it on **Scenes and Sources**, choose **Sends...**, and select a bus. Leave **Send directly to master** enabled for a dry signal plus bus effects; disable it when the source should be heard only through its buses.

## The media player

A **Media Player** source plays a folder of your own music into the mix. Add one on **Scenes and Sources** (**Add source**, then **Media Player**) and choose a folder in its dialog. Like any other source it has a volume slider, mute, monitoring and bus sends, and what you monitor is exactly what listeners get, ducking included.

Everything in the folder is played, subfolders included, so a whole music library works as well as a single album. Pubsplash plays MP3, M4A, MP4, AAC, FLAC, OGG, WAV, AIFF, CAF, MKA and MP1/MP2; anything else, Opus and WMA among them, is ignored rather than queued and then skipped in silence. The folder is read again every few minutes, so music you add during a session joins in without restarting anything.

**Shuffle the folder** is on by default: every file plays once before any of them repeats, the folder is reshuffled when it runs out, and a new round never starts with the track that just finished. Turn it off to play in filename order instead. A media player starts when its scene goes live and stops when you switch away, the same rule the microphones follow, and the Sources list shows what it is playing.

### Talking over it

**Turn the music down while other sources are playing** is on by default. The music drops as soon as another source in the scene makes a sound and comes back up about a second after it stops, so you can talk over it, or let chat be read over it, without touching a fader.

**Turned-down level** is how far it drops, as a percentage of this source's own volume slider; 0% silences the music completely while anything else is playing. Microphones, text-to-speech and sound events all count as something playing. Other media players do not, so two of them never fight each other, and a muted source, or one pulled down to zero, does not turn the music down either.

**Start turning down at** is how loud something has to get before it counts, from -60 dB (almost anything) to -10 dB (only a shout). It ships at -30 dB: the level of somebody deliberately talking, and far enough above a breath across the microphone, a fan, or a knock on the desk that none of those duck your music. **Calibrate to my voice** sets it for you. Press it and talk normally for five seconds, and Pubsplash measures the same signal the ducking watches - your live scene, through its faders and mutes - then puts the level a little way under what it heard. Make sure the microphone you are calibrating is in that scene and unmuted; if nothing loud enough to be a voice arrives, it says so and changes nothing.

If the music stays turned down when nobody is talking, something in the scene is genuinely making noise: a **Desktop Audio** source counts, so a video in a background tab ducks the music for as long as it runs.

### Playing, pausing and skipping

The transport is on the media player's mixer strip. Open the volume slider's context menu and choose **Play** / **Pause**, **Next track**, or **Open file**. All three can be bound to shortcuts of your own on **Preferences > Keybinds**, which is what you want mid-broadcast, since a shortcut works from any tab. Skipping announces the track it moved to rather than the fact that you pressed it.

**Open file** plays one file of your choosing from anywhere on your computer; it does not have to be in the source's folder, and nothing about it is remembered. It interrupts what is playing exactly as a skip does, and when it ends the folder carries on with the track it was going to play next. There is a button for it on the strip beside the mute box, and the first media player you add is given `Ctrl+O` for it.
## Chat and text-to-speech

The **Chat** tab shows incoming Audiopub messages and lets you send replies. The feed reconnects automatically if it drops. **Reconnect chat** forces an immediate reconnect without interrupting your stream.

Note: The reconnect chat button is there as a means of trying to work around a server-side issue we have no control over. It may not work in all instances.

To read chat aloud, add a **Text-to-Speech** source. SAPI 5, Microsoft Edge, and Google Translate need no API credentials. OpenAI, ElevenLabs, Azure, AWS Polly, Google Cloud, and a self-hosted Star server require credentials on the **Speech** tab of Preferences. Credentials are encrypted for your Windows account.

Note: Star support should be considered inoperative at the current time.

Speech is played locally by default. Enable **Send speech to the stream** if listeners should hear it too. The Speech tab also controls message length and the delay between requests. The **API** tab shows usage for engines that have spoken during the current session.

## Keyboard access

| Shortcut | Action |
| --- | --- |
| F1 | Help for the focused control |
| F6 / Shift+F6 | Move between lists on the current tab |
| F9 / F10 | Start or stop streaming / recording |
| Ctrl+, | Open Preferences |
| Ctrl+M | Toggle monitoring for the focused mixer strip |
| Ctrl+O | Open a file on your first media player |

Use **Preferences > Keybinds** to add, change, or remove shortcuts. Global shortcuts can work while another application is focused; they must include Ctrl, Alt, or Shift, or be a function key.

Mixer sliders change by 1% with arrow keys and by 10% with Page Up or Page Down. Home and End move to maximum and minimum. A slider's context menu can enable volume boost up to 500%.

## Choosing devices

The **Audio** tab of Preferences chooses which playback device Pubsplash plays out of. Everything Pubsplash plays for you goes there - sources you are monitoring, text-to-speech, and sound cues - so you can monitor on headphones while the rest of the machine keeps using the speakers. It is not what your listeners hear. The default, **Default output device (follow system)**, uses whatever Windows is currently using, so it moves with you when you plug in a headset. **Play a test sound** checks your choice without starting a stream.

A **Desktop Audio** source captures all of your playback devices at once by default, leaving out Pubsplash's own audio. Its **Edit** dialog can instead pin it to one device by name. There is one rule: that device cannot be the one Pubsplash plays out of. Capturing a single device captures *everything* on it with nothing left out, so aiming it at Pubsplash's own output would send your speech and sound cues straight back to your listeners. Pubsplash refuses that pairing and says so, leaving the dialog open so you can pick another - and if you later change the output device to one a Desktop Audio source is capturing, it tells you and that source falls back to capturing every device with Pubsplash excluded. Either way, Pubsplash's own audio never reaches your stream.

## Optional features

### Mastodon announcements

On the **Mastodon** tab of Preferences, choose **Authorize** and approve Pubsplash in your browser. You can then post when a stream starts or periodically. Templates support {title}, {description}, {url}, and {tod}. Every automated post ends with #PubsplashStreamInfo to make it easier for your followers to manage.

### Sound packs

The **Sound packs** tab controls startup, shutdown, listener, and chat sounds. You can import .pspack files, preview their events, and choose a pack. **Tools > Sound Pack Manager** creates and compiles packs; pack projects can contain WAV and Ogg Opus files.

### Automatic updates

Pubsplash checks for updates at startup by default. Change this on Preferences' **General** tab, or use **Check for updates now**. Updates are verified before installation and never interrupt an active stream or recording.

## Troubleshooting

If a source is silent, check its device or application selection and look for "(reconnecting)" in the mixer. For connection problems, verify the service credentials and consult the log.

If listeners hear nothing at the start of a broadcast, read the Status line on the **Home** tab. **"Streaming (waiting for the server to accept the stream)"** means your audio is going out but Audio Pub has not finished checking it yet, and a stream page will play silence until it does — this normally clears in a few seconds. **"Streaming (the server has lost the source)"** means the server has stopped receiving you even though your own connection looks healthy; it will end the stream in a few minutes if that does not recover. A stream that stays unaccepted for three quarters of a minute is explained in the log.

Open **Go to > Go to Pubsplash data directory** to find the data folder. Logs are in %LOCALAPPDATA%\pubsplash\logs\. On Preferences' **Logging & debugging** tab, increase the log level temporarily or choose **Compress logs** to create a ZIP containing logs and crash dumps for a bug report. The archive does not include settings, passwords, or API keys.

## Building from source

Install Rust stable, Visual Studio 2019 or later with the Windows SDK, CMake, and Ninja. Then run:

    cargo build --release

The first build downloads the required prebuilt wxWidgets libraries.

## License

See [LICENSE](LICENSE) for licensing information.

