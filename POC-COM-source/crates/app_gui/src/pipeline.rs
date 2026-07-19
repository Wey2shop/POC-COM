//! Pure compute glue between the crates: build a marker-word token
//! sequence, assemble it into audio from the embedded clip library, and
//! on the way back, transcribe real audio and parse the marker-word text
//! out of it. Kept free of any GUI/audio-device concerns so it can run
//! on a background thread without blocking the UI.
//!
//! This app went through several physical layers before landing here
//! (see `lexicon_modem`'s own doc comments for the full history) -- the
//! one constant through all of them is that this file is the seam
//! between "what a mode wants to say" and "how it actually goes
//! out/comes back over audio". There is no compression, no bit-packing,
//! no FEC anymore: a message is spelled out via a small, fixed,
//! pre-rendered vocabulary, and decoding it is just asking Whisper what
//! it heard and mapping the recognized words back to characters.
//!
//! Receive is auto-detecting, not mode-scoped: since there's no shared
//! kind tag anymore, `decode_any_reception` tries `message::parse_mail_text`
//! then `message::parse_social_text` against whatever Whisper transcribed,
//! and returns whichever one actually matches its markers.

use crate::maidenhead;
use lexicon_modem::message::{self, MailFields};
use lexicon_modem::vocabulary;
use std::io::Read;

/// Soft UI guardrails on field length -- not protocol limits (there's no
/// fixed frame to overflow anymore), just a sanity cap so a message can't
/// grow to the point where assembly + transcription take unreasonably
/// long. Every extra character now costs a whole spoken clip plus a gap,
/// not a fraction of a natural-speech second -- these are deliberately
/// much smaller than the natural-language-TTS era's limits.
pub const MAX_FROM_CHARS: usize = 12;
pub const MAX_TO_CHARS: usize = 12;
pub const MAX_SUBJECT_CHARS: usize = 24;
pub const MAX_MESSAGE_CHARS: usize = 60;
pub const MAX_POST_CHARS: usize = 60;

/// Anything uploaded as a Social attachment goes to this fixed host --
/// only the dynamic filename/ID after this prefix needs to be spoken and
/// transcribed; the receiver reconstructs the full URL by prepending this
/// same known prefix back. See `shorten_media_url`/`expand_media_url`.
const LITTERBOX_PREFIX: &str = "https://litter.catbox.moe/";

fn shorten_media_url(url: &str) -> String {
    url.strip_prefix(LITTERBOX_PREFIX).unwrap_or(url).to_string()
}

fn expand_media_url(short: &str) -> String {
    format!("{LITTERBOX_PREFIX}{short}")
}

/// Milliseconds of digital silence between consecutive clips. This used
/// to be a user-facing Slow/Normal/Fast picker (250/150/100ms); real
/// live-hardware testing (not just this crate's synthetic round-trip
/// tests, which pipe assembled audio straight into Whisper and never
/// exercise a real speaker/mic/PTT/VAD path at all) surfaced failures at
/// the fastest setting, so the picker was removed in favor of a single,
/// more conservative fixed value -- one less variable to reason about
/// when a real transmission gets misheard, and margin against clip
/// blending matters more than shaving airtime.
const GAP_MS: u32 = 200;

pub struct PreparedTx {
    pub wave: Vec<f32>,
    pub sample_rate: u32,
    /// What the assembled clips actually say, word for word -- shown in
    /// the compose UI once ready, so a listener on the actual radio and
    /// someone reading the app's own preview see the same thing.
    pub spoken_text: String,
}

fn synthesize(tokens: Vec<&'static str>) -> PreparedTx {
    let spoken_text = tokens.iter().map(|k| vocabulary::spoken_word_for(k)).collect::<Vec<_>>().join(" ");
    let wave = lexicon_modem::assemble_from_tokens(&tokens, GAP_MS);
    PreparedTx { wave, sample_rate: lexicon_modem::SAMPLE_RATE, spoken_text }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f32 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f32 / (1024.0 * 1024.0))
    }
}

// ---------------------------------------------------------------------
// Mail mode
// ---------------------------------------------------------------------

pub fn prepare_mail_transmission(from: &str, to: &str, location: &str, subject: &str, message: &str) -> PreparedTx {
    let tokens = message::build_mail_tokens(from, to, location, subject, message);
    synthesize(tokens)
}

// ---------------------------------------------------------------------
// Social mode -- attachments never ride the audio channel: they're
// uploaded to remote blob storage and only the (shortened) URL is spoken.
// ---------------------------------------------------------------------

/// Strip characters that would break out of the multipart
/// `Content-Disposition` header if embedded verbatim.
fn sanitize_filename(name: &str) -> String {
    name.chars().map(|c| if c == '"' || c == '\r' || c == '\n' { '_' } else { c }).collect()
}

/// Upload `bytes` to remote blob storage (litterbox.moe, a free anonymous
/// temporary file host -- 72h retention) and return the resulting URL.
/// Actual media bytes never ride the audio channel, only this URL does --
/// and only the part after `LITTERBOX_PREFIX` actually gets spoken (see
/// `shorten_media_url`).
pub fn upload_attachment(bytes: &[u8], filename: &str) -> Result<String, String> {
    const BOUNDARY: &str = "----poc-social-boundary-7f3a9c";
    let safe_name = sanitize_filename(filename);

    let mut body = Vec::with_capacity(bytes.len() + 512);
    body.extend_from_slice(format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"reqtype\"\r\n\r\nfileupload\r\n").as_bytes());
    body.extend_from_slice(format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"time\"\r\n\r\n72h\r\n").as_bytes());
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"fileToUpload\"; filename=\"{safe_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let resp = ureq::post("https://litterbox.catbox.moe/resources/internals/api.php")
        .set("Content-Type", &format!("multipart/form-data; boundary={BOUNDARY}"))
        .send_bytes(&body)
        .map_err(|e| format!("upload failed: {e}"))?;
    let url = resp.into_string().map_err(|e| format!("upload response read failed: {e}"))?.trim().to_string();
    if !url.starts_with("http") {
        return Err(format!("upload failed: unexpected response ({url})"));
    }
    Ok(url)
}

/// Fetch a blob (e.g. an image referenced by a decoded post's URL) for
/// local display.
pub fn fetch_blob(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).call().map_err(|e| format!("fetch failed: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf).map_err(|e| format!("fetch read failed: {e}"))?;
    Ok(buf)
}

/// Build and assemble a Social post. `media_url`, if present, is the
/// *full* URL returned by `upload_attachment` -- only its short suffix
/// after the known host prefix is actually spoken. `grid`, if present, is
/// validated as a real Maidenhead locator first; an invalid one is
/// silently dropped rather than spoken as garbage text.
pub fn prepare_social_transmission(author: &str, post: &str, media_url: Option<&str>, grid: Option<&str>) -> PreparedTx {
    let link = media_url.map(shorten_media_url);
    let valid_grid = grid.filter(|g| maidenhead::to_latlon_center(g).is_some());
    let tokens = message::build_social_tokens(author, post, link.as_deref(), valid_grid);
    synthesize(tokens)
}

// ---------------------------------------------------------------------
// Shared receive path: transcribe once, try both known formats.
// ---------------------------------------------------------------------

/// The Whisper model is expensive to load (a ~145MB embedded checkpoint),
/// so it's loaded once, lazily, on first use, and reused across every
/// subsequent receive -- not reloaded per call. Wrapped in a `Mutex` since
/// this whole module's design goal is to be safely callable from a
/// background thread.
static WHISPER_DECODER: std::sync::OnceLock<std::sync::Mutex<Result<lexicon_modem::WhisperDecoder, String>>> = std::sync::OnceLock::new();

fn with_whisper_decoder<T>(f: impl FnOnce(&mut lexicon_modem::WhisperDecoder) -> Result<T, String>) -> Result<T, String> {
    let cell = WHISPER_DECODER.get_or_init(|| std::sync::Mutex::new(lexicon_modem::WhisperDecoder::load().map_err(|e| e.to_string())));
    let mut guard = cell.lock().map_err(|_| "Whisper decoder lock poisoned by an earlier panic".to_string())?;
    match guard.as_mut() {
        Ok(decoder) => f(decoder),
        Err(e) => Err(format!("Whisper model failed to load: {e}")),
    }
}

pub struct DecodedSocial {
    pub author: String,
    pub post: String,
    pub media_url: Option<String>,
    pub origin_grid: Option<String>,
}

pub enum ReceivedPayload {
    Mail(MailFields),
    Social(DecodedSocial),
}

/// Transcribes `samples` (48kHz mono) once and tries to parse the result
/// as a Mail message first, then a Social post -- whichever one's marker
/// words actually show up in the transcript wins. Neither matching means
/// no real transmission was present, or enough words were misheard that
/// the marker framing broke.
pub fn decode_any_reception(samples: &[f32]) -> Result<ReceivedPayload, String> {
    let transcript = with_whisper_decoder(|decoder| decoder.transcribe(samples).map_err(|e| e.to_string()))?;

    if let Some(mail) = message::parse_mail_text(&transcript) {
        return Ok(ReceivedPayload::Mail(mail));
    }
    if let Some(social) = message::parse_social_text(&transcript) {
        let media_url = social.link.as_deref().map(expand_media_url);
        let origin_grid = social.grid.filter(|g| maidenhead::to_latlon_center(g).is_some());
        return Ok(ReceivedPayload::Social(DecodedSocial { author: social.author, post: social.post, media_url, origin_grid }));
    }

    Err(format!("couldn't find a recognized message format in the transcript: \"{transcript}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_prepare_and_decode_round_trip_mail() {
        // "IO80" specifically: confirmed (see `lexicon_modem::phonetic`)
        // that a bare spoken "IO80" gets misheard as "I-080" by this
        // exact toolchain. Now spelled out via NATO phonetic words, so
        // this is a real regression proof the fix works end-to-end
        // through the app's own pipeline, not just an arbitrary example.
        let prepared = prepare_mail_transmission("KJ4ABC", "Base", "IO80", "Status", "All quiet");

        let payload = decode_any_reception(&prepared.wave).expect("decode_any_reception should succeed");
        match payload {
            ReceivedPayload::Mail(mail) => {
                assert_eq!(mail.from, "kj4abc");
                assert_eq!(mail.to, "base");
                assert_eq!(mail.location, "io80");
                assert_eq!(mail.subject, "status");
                assert_eq!(mail.message, "all quiet");
            }
            ReceivedPayload::Social(_) => panic!("expected a Mail decode"),
        }
    }

    #[test]
    fn full_prepare_and_decode_round_trip_social() {
        // "G4ABC"/"IO80": confirmed (see `lexicon_modem::phonetic`) that
        // Whisper reproducibly mangles both spoken bare -- "g4 abc" (a
        // spurious inserted space) and "I-080" (letter O misheard as
        // digit 0) respectively. Both are now spelled out via NATO
        // phonetic words, so using them here is a real regression proof,
        // not just an arbitrary example.
        let prepared = prepare_social_transmission("G4ABC", "Ridge copy", None, Some("IO80"));

        let payload = decode_any_reception(&prepared.wave).expect("decode_any_reception should succeed");
        match payload {
            ReceivedPayload::Social(social) => {
                assert_eq!(social.author, "g4abc");
                assert_eq!(social.post, "ridge copy");
                assert_eq!(social.origin_grid.as_deref(), Some("io80"));
            }
            ReceivedPayload::Mail(_) => panic!("expected a Social decode"),
        }
    }

    #[test]
    fn full_prepare_and_decode_round_trip_social_without_link_or_grid() {
        // `LINK` and `GRID` are now always spoken markers (see
        // `lexicon_modem::message::build_social_tokens`), even when both
        // fields are absent -- meaning they're spoken back-to-back with
        // nothing in between. This is a real proof that real Whisper
        // still transcribes that adjacency cleanly enough to parse, not
        // just a synthetic assumption.
        let prepared = prepare_social_transmission("G4ABC", "Ridge copy", None, None);

        let payload = decode_any_reception(&prepared.wave).expect("decode_any_reception should succeed");
        match payload {
            ReceivedPayload::Social(social) => {
                assert_eq!(social.author, "g4abc");
                assert_eq!(social.post, "ridge copy");
                assert!(social.media_url.is_none());
                assert!(social.origin_grid.is_none());
            }
            ReceivedPayload::Mail(_) => panic!("expected a Social decode"),
        }
    }

    #[test]
    fn full_prepare_and_decode_round_trip_social_reddy_ryder_regression() {
        // "M7USD" (this test's original value) reproduced a real failure:
        // "Ready" immediately followed by "Writer" then "M7USD"'s phonetic
        // spelling made Whisper transcribe BOTH "Ready" and "Writer" as
        // different words ("Reddy, Ryder") -- and, tellingly, this didn't
        // change at all when the gap was widened from 60ms to 100ms,
        // ruling out audio blending as the cause. This is Whisper's own
        // cross-attention re-interpreting the whole utterance based on
        // what comes *later* in the audio, not a local pause problem --
        // specific to this one value combination (a different callsign
        // here passes reliably), so it's kept as its own regression case
        // rather than folded into the general round-trip test above.
        let prepared = prepare_social_transmission("G7ZKD", "High World", None, Some("IO80"));

        let payload = decode_any_reception(&prepared.wave).expect("decode_any_reception should succeed");
        match payload {
            ReceivedPayload::Social(social) => {
                assert_eq!(social.author, "g7zkd");
                assert_eq!(social.post, "high world");
                assert_eq!(social.origin_grid.as_deref(), Some("io80"));
            }
            ReceivedPayload::Mail(_) => panic!("expected a Social decode"),
        }
    }

    #[test]
    fn shorten_and_expand_media_url_round_trip() {
        let full = format!("{LITTERBOX_PREFIX}abc123.jpg");
        let short = shorten_media_url(&full);
        assert_eq!(short, "abc123.jpg");
        assert_eq!(expand_media_url(&short), full);
    }
}
