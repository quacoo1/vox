# Vox

Select text anywhere on Windows, pick **Read with Vox**, and hear it read aloud by an
[ElevenLabs](https://elevenlabs.io) voice. A small floating player shows the sentence being spoken with the current
word highlighted, and the main window keeps your history, listening analytics, voice library and settings.

This is a native rewrite of the Electron app in Rust with a [Slint](https://slint.dev) UI. There is no web view and no
Node runtime: one `vox.exe` plus the unchanged Chrome/Edge extension.

## Requirements

- Windows 10 or 11 (x64)
- Rust 1.98+ with the MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`)
- An ElevenLabs API key (entered in the app on first run; it is stored encrypted with Windows DPAPI under
  `%APPDATA%\Vox`, never in this repository)

## Build and run

```powershell
cargo build -p vox-app                # debug: target\debug\vox.exe
cargo build -p vox-app --release      # release: target\release\vox.exe
cargo test --workspace
```

The build copies `browser-extension\` next to `vox.exe`.

Try it without an API key or network access:

```powershell
$env:VOX_DEMO = '1'; .\target\debug\vox.exe
```

Demo mode uses a separate profile (`%APPDATA%\Vox-Demo`) with seeded history and a mock voice.

Command line: `vox.exe --read-text "Hello"`, `--read-clipboard`, `--read-file <path>`, `--hidden` (start in the
tray). A second launch hands its arguments to the copy that is already running.

## Voice providers

Besides ElevenLabs, Vox can read with voices from other services. Open **Settings → Voice providers**, paste a key
and click **Connect**; Vox checks the key, and the service's voices then appear on the **Voices** page (filter by
type, for example "Amazon Polly"). **Preview** plays a short sample; **Use voice** picks it.

| Service | What you need | Word highlighting |
|---|---|---|
| **Windows** | Nothing: the voices built into Windows are always there (add more in Windows Settings → Time & language → Speech) | Exact |
| **Amazon Polly** | An IAM access key ID + secret access key with the `AmazonPollyReadOnlyAccess` permission, and a region | Exact (speech marks) |
| **Azure AI Speech** | A Speech resource key and its region | Estimated |
| **OpenAI** | An API key | Estimated |
| **Google Cloud** | An API key for a project with the Text-to-Speech API enabled | Estimated |

Each service bills its own account; their usage never counts against your ElevenLabs credits. Polly voices are listed
as their neural version plus a separate "(Generative)" entry where AWS has one, and each Polly chunk makes one audio
request and one speech-marks request. Google lists its Chirp 3 HD, Chirp HD, Studio, Neural2, Journey and Polyglot
voices. Keys are stored encrypted with Windows DPAPI in `%APPDATA%\Vox\providers-dpapi.bin` and never written to logs.

## Building the installer

The installer is made with [Inno Setup](https://jrsoftware.org/isinfo.php). It is a per-user install (no
administrator rights) into `%LOCALAPPDATA%\Programs\Vox`.

1. Install Inno Setup once:

   ```powershell
   winget install JRSoftware.InnoSetup
   ```

2. Build the release exe:

   ```powershell
   cargo build -p vox-app --release
   ```

3. Compile the installer script:

   ```powershell
   & "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" installer\vox.iss
   ```

   winget installs Inno Setup per user. If you installed it for all users instead, the compiler is at
   `C:\Program Files (x86)\Inno Setup 6\ISCC.exe`. In PowerShell the `&` in front of the quoted path is required.

The result is `dist\Vox-Setup-<version>.exe`. Builds are not code-signed, so Windows SmartScreen shows a warning;
choose **More info → Run anyway**, or sign the exe and installer with a code-signing certificate.

Quit the old Electron Vox before installing or running this build: both use the same profile, and the Rust app
refuses to start while the Electron one is running.

## Browser extension

Open `chrome://extensions` (or `edge://extensions`), turn on **Developer mode**, click **Load unpacked** and pick
the `browser-extension` folder next to `vox.exe`. It talks only to Vox on `127.0.0.1:47823`.

## Project layout

```
crates/
  vox-core     shared types, settings, formatting, hotkeys (no I/O)
  vox-text     text clean-up, chunking, sentence/word timing, file text extraction
  vox-store    settings, history, audio cache, analytics, API-key storage, logs
  vox-tts      ElevenLabs client and synthesis engine
  vox-audio    decoding, pitch-preserving speed, playback
  vox-win      global hook, selection capture, clipboard, hotkeys, media keys, registry
  vox-bridge   localhost bridge for the browser extension
  vox-art      procedural "silk" art for voices and the backdrop
  vox-app      the vox.exe binary: core thread, windows, tray, UI glue
ui/            Slint UI: design system, pages, windows, previews
tools/vox-shot renders any .slint file with JSON data to a PNG
docs/          BRIEF, DESIGN, ARCHITECTURE, STATUS and the behaviour specs in docs/port/
```

See [docs/STATUS.md](docs/STATUS.md) for what is verified and what still needs manual testing.
