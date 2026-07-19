
Official Website: [POC-COM Project Page](https://wey2shop.github.io/POC-COM)

## Just looking for the build. 

*   [Download for Windows (≈294 MB)](https://github.com/Wey2shop/POC-COM/releases/download/POC-COM/poc-com-windows-x86_64.exe)
*   [Download for Linux (≈297 MB)](https://github.com/Wey2shop/POC-COM/releases/download/POC-COM/poc-com-linux-x86_64)


# POC-COM

**Send structured messages and open-board posts over a Push-to-Talk-over-Cellular (PoC) radio using nothing but the voice channel — carried as real speech, with no TTS engine running at send time.**

POC-COM is a small, self-contained desktop app (Windows and Linux) that
turns a message into a sequence of real, pre-recorded speech clips, plays
it out a PTT radio's mic input, and transcribes it back out of whatever
comes out the radio's speaker on the other end (a real Whisper
speech-recognition model, running fully offline). It exists because most
PoC radios and apps (Zello and similar) expose *only* a voice audio path —
no data channel, no API, no Bluetooth pairing to the app. If the radio can
carry a voice call, POC-COM can carry a message.

One app, two modes, switchable from a single header at any time:

- **Mail** — structured messages: From / To / Location / Subject / Message.
- **Social** — an open, unsigned board: short posts, with an optional
  attachment uploaded to remote blob storage first, so only a short link
  needs to be spoken over the air.

There are no volume or gain controls anywhere in this app, and no audio
passthrough/monitoring path. That's a structural choice, not a missing
feature — see [Design principles](#design-principles).

## How it works

See the [technical writeup](docs/lexicon_modulation.html) for the full
story of how this app got here (it went through four very different
designs before landing on this one), but the current design in one
paragraph:

A message's fields are spelled out as a sequence of tokens — NATO
phonetic letters, digit words, and marker words — and each token maps to
a short, real speech clip that was recorded exactly once, offline, and is
embedded in the binary. Sending a message is pure sample-buffer
concatenation with a precise, tunable silence gap between clips: no TTS
engine, no OS-specific speech API, and no external process runs at send
time, on either platform. Whatever the radio's speaker outputs on the
other end gets transcribed by a real Whisper (`base.en`) model, in its
ordinary free-form mode — no constrained vocabulary, no bit-packing, no
forward error correction. The transcript is searched for the marker
words, and the text between them, spelled back out letter by letter, *is*
the message. Anyone listening on the actual radio hears the same spoken
letters and words a person reading them phonetically would say — that's
not an accident, it's the entire design: stop fighting the voice codec
and the speech-recognition model's own instincts, and speak in a small,
fixed vocabulary both systems handle reliably.

## What it looks like

A shared header (device pickers, dark-mode toggle, waterfall toggle) and a
Mail / Social switcher, then each mode's own compose panel (left) and
result list (center):

- **Compose** (mode-specific): Mail fills in From/To/Location/Subject/
  Message; Social writes a short post plus an optional attachment (uploaded
  to remote storage on Post, with only its short link spoken aloud). Both
  share the same Prepare → 3-second PTT countdown → Transmit flow. There's
  no user-facing pause-length picker anymore — an earlier Slow/Normal/Fast
  choice was collapsed to one fixed, more conservative gap after real
  live-hardware testing (not just this app's own synthetic round-trip
  tests) surfaced failures at the fastest setting.
- **Listening is shared across both modes, not per-mode.** There's only
  one input device, so there's only one "Start Listening" — visible
  regardless of which tab is open. A decoded transmission is recognized by
  which marker words actually show up in its transcript and routed
  straight into the matching mode's list: a Mail message sitting on the
  Social tab still lands in the Mail inbox, and vice versa.
- An optional embedded SDR-style waterfall panel (toggle button top-right,
  `Esc` to close) shows the live speech signal exactly as it's being sent
  or received, docked on the far right.

## Building

Windows targets a GNU/MinGW Rust toolchain (this project pins
`x86_64-pc-windows-gnu` in [`.cargo/config.toml`](.cargo/config.toml) — see
[WinLibs](https://winlibs.com/) for a POSIX UCRT toolchain that works well
on Windows). Linux builds with a normal native toolchain — the shipped app
has no Windows-specific dependency anywhere in its default feature set.

```sh
cargo build --release -p app_gui
```

The resulting binary (`poc_com.exe` on Windows, `poc_com` on Linux) is
self-contained — no installer, no external DLLs/shared libraries to ship
alongside it (the Whisper model and the entire speech clip library are
both embedded in the binary).

Run the full test suite (unit tests plus real end-to-end pipeline round
trips — real clip assembly piped into a real Whisper decode, for both
modes) with:

```sh
cargo test --release --workspace
```

These real-model tests are slow (each one runs a real Whisper forward
pass) — expect a couple of minutes for the full suite, not seconds.

### Regenerating the speech clip library (Windows only, rarely needed)

The clip library (`crates/lexicon_modem/assets/clips.bin`) is pre-rendered
once and checked into the repo — normal builds never touch a speech
engine. If the vocabulary ever needs to change, regenerate it on Windows
with:

```sh
cargo run --release --example generate_clips -p lexicon_modem --features gen-clips
```

then re-run the full test suite against the new file before committing it
— clip generation isn't perfectly reproducible run to run, so every
regenerated set needs its own pass through the real round-trip tests.

## Project layout

This is a Cargo workspace of three main crates:

| Crate | Responsibility |
|---|---|
| [`lexicon_modem`](crates/lexicon_modem) | The embedded speech clip library (`vocabulary.rs` for the word list, `clips.rs` for lookup, `modulator.rs` for pure-Rust assembly), real ASR transcription (`decoder.rs`, Whisper `base.en` via `candle`, fully embedded, no runtime network dependency), spelling logic (`phonetic.rs`), the marker-word message format (`message.rs`), and the Windows-only, opt-in clip generator (`dev_tts.rs` + `examples/generate_clips.rs`, gated behind the `gen-clips` feature, never built by default) |
| [`audio_io`](crates/audio_io) | cpal-backed device enumeration and TX/RX streaming — deliberately has no gain parameter and no passthrough path anywhere in its API |
| [`app_gui`](crates/app_gui) | The eframe/egui desktop application — see below |

Inside `app_gui`, each mode's compose/result UI is its own module
(`mail_ui.rs` for Mail, `board_ui.rs` for Social), all built on the same
shape: a `*State` struct plus a render function, driven by
[`pipeline.rs`](crates/app_gui/src/pipeline.rs) (pure compute: build the
marker-word token sequence and assemble it into audio for sending,
transcribe and parse it for receiving) and
[`listen.rs`](crates/app_gui/src/listen.rs) (the one shared listening
session both modes route through — `pipeline::decode_any_reception`
transcribes once and tries both modes' marker-word parsers against the
result, and `app.rs` routes whichever one matches into that mode's list).

## Design principles

- **Voice-channel only.** No assumption of a data channel, API, or paired
  app on the other end — anything that can carry a voice call can carry
  POC-COM traffic.
- **No volume controls, no passthrough.** `audio_io`'s public API has no
  gain parameter, and TX/RX are structurally separate — there's no code
  path that reads an input stream and writes it to an output stream. This
  keeps the app from ever being usable as a covert listening/relay tool,
  by construction rather than by policy.
- **Ship on every platform you promise.** An earlier version of this
  design used live TTS synthesis (first a PowerShell/SAPI subprocess,
  later in-process WinRT) — it worked, but was Windows-only, and this
  project needs a real Linux build too. Pre-rendering a small fixed
  vocabulary once and shipping it as embedded data removed the runtime
  TTS dependency entirely, on both platforms.
- **Stop fighting the channel — become the signal it's built to carry.**
  Every earlier physical layer this project tried (tone chirps, on/off
  tone chords, multi-lane FSK, a closed dictionary of isolated TTS words)
  fought either the voice codec (tuned to preserve *speech*, and to
  reshape or discard anything that doesn't look like it) or Whisper's own
  language-model prior (which "corrects" disconnected words into
  fluent-sounding nonsense). Real speech clips, spoken in a small,
  well-tested vocabulary, sidestep both fights at once — see
  [the technical writeup](docs/lexicon_modulation.html) for the full
  history.
- **Marker words, not fixed frames.** There's no bit-packing, no checksum,
  no block/frame structure. A message is exactly the words a listener
  would hear, with marker words doing the only structural work: showing a
  parser where one field ends and the next begins.
- **One shared receive path, auto-detecting by content.** Both modes'
  transmissions are transcribed by the exact same call; which mode a
  transcript belongs to is decided by which marker words are actually
  found in it, not by any tag carried alongside the audio.

## Current limits

- Every character of every field now costs one whole spoken clip plus a
  gap, not a fraction of a natural sentence — field lengths are capped at
  deliberately short values (see `pipeline.rs`'s `MAX_*_CHARS` constants)
  to keep transmissions to a reasonable length.
- No forward error correction at all. A badly misheard marker word (rare,
  but not impossible) means that field — or the whole message, if a marker
  itself is missed — fails to parse; there's no partial-credit "signal
  quality" score anymore, just parse-succeeds or parse-fails.
- No history replay across devices in Mail/Social — a new listener only
  sees transmissions that arrive while they're actively listening, same as
  a real radio. In-session history persists only for as long as the app is
  running.
- Social: one attachment per post, uploaded to litterbox.moe with 72h
  retention; no inline video/audio playback (link-out instead, by design).
- The Linux build has no Windows-specific dependency in its default
  feature set (confirmed: a normal `cargo build`/`cargo test` pulls in no
  `windows` crate at all), and has been built and fully test-verified on
  a real Linux machine (WSL Ubuntu) — the full workspace test suite,
  including every real end-to-end round trip, passes natively there too.

## License

MIT — see [LICENSE](LICENSE).
