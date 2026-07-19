//! Open-board UI for POC-COM_SOCIAL: a compose panel (left) for authoring
//! a post -- text plus an optional single attachment -- and a feed panel
//! (center) that continuously listens and lists what's been received,
//! chat-board style: oldest at the top, newest at the bottom, auto-
//! scrolling as new posts arrive. There are no accounts and no encryption
//! -- this is an open ledger, readable by anyone in range, same as the
//! audio itself already is.
//!
//! Attachments never ride the audio channel: they're uploaded to remote
//! blob storage (`pipeline::upload_attachment`) and only the resulting
//! URL's short, dynamic suffix travels over the air, spoken aloud as part
//! of the post (`pipeline::prepare_social_transmission`).

use crate::pipeline::{fetch_blob, format_bytes, prepare_social_transmission, upload_attachment, PreparedTx};
use crate::settings::Identity;
use crate::theme::{self, ACCENT, ERROR, WARNING};
use crate::wav;
use crate::waterfall::WaterfallWindow;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PTT_LEAD_IN: Duration = Duration::from_secs(3);
/// Wait after keying a serial PTT port for the radio's relay to
/// physically close before audio starts -- mirrors MMSSTV's own
/// `TX;\w10` command template, just longer (confirmed via real testing
/// that both 10ms and 250ms cut it too close for comfort). See
/// `audio_io::ptt`.
const PTT_PRE_DELAY: Duration = Duration::from_millis(750);
/// Held after the audio itself finishes, before PTT actually releases --
/// covers OS/soundcard output buffering, which can report "finished"
/// slightly before the last samples have physically left the speaker.
const PTT_POST_DELAY: Duration = Duration::from_millis(400);

/// A generous but finite sanity cap -- attachment bytes never ride the
/// audio channel (only the resulting URL's short suffix is spoken), so
/// there's no airtime reason to limit this; it just keeps a stray
/// multi-gigabyte pick from hanging the upload.
const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

pub enum SendPhase {
    Idle,
    /// Covers both the (optional) attachment upload and TTS synthesis --
    /// collapsed into one phase because both happen in the same
    /// background thread and a post is small enough that splitting the UI
    /// state for them isn't worth the complexity.
    Preparing { rx: mpsc::Receiver<Result<PreparedTx, String>> },
    Prepared { prepared: PreparedTx },
    Countdown { prepared: PreparedTx, deadline: Instant },
    /// `ptt` is `None` unless a serial PTT port is configured in
    /// Settings -- when present, it's held open for the whole
    /// transmission (and through `Releasing` below).
    Transmitting { tx: audio_io::TxHandle, ptt: Option<audio_io::PttPort> },
    /// Audio has finished playing but a configured PTT connection is
    /// held keyed a little longer (`PTT_POST_DELAY`) before its `Drop`
    /// impl actually sends the Kenwood `RX;` key-down command --
    /// `_ptt` is only ever held for that side effect, never read
    /// directly (skipped entirely when `None`, since manual PTT has
    /// nothing here to hold open).
    Releasing { until: Instant, _ptt: Option<audio_io::PttPort> },
    Done,
    Failed(String),
}

pub struct ComposeState {
    pub post: String,
    pub attachment: Option<(PathBuf, Vec<u8>)>,
    attachment_warning: Option<String>,
    pub phase: SendPhase,
    cached_wave: Option<(Vec<f32>, u32)>,
    /// Set if a configured serial PTT port failed to key at the moment a
    /// transmission started -- the transmission still proceeds (manual
    /// PTT is always the fallback), this is just surfaced so a silently
    /// unkeyed radio doesn't go unnoticed.
    ptt_warning: Option<String>,
}

impl Default for ComposeState {
    fn default() -> Self {
        Self {
            post: String::new(),
            attachment: None,
            attachment_warning: None,
            phase: SendPhase::Idle,
            cached_wave: None,
            ptt_warning: None,
        }
    }
}

impl ComposeState {
    fn reset_for_next_post(&mut self) {
        self.post.clear();
        self.attachment = None;
        self.attachment_warning = None;
        self.phase = SendPhase::Idle;
        self.cached_wave = None;
    }
}

pub fn compose_ui(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut ComposeState,
    output_device: Option<&str>,
    waterfall: &mut WaterfallWindow,
    identity: &Identity,
) {
    ui.heading("📡 Compose");
    ui.add_space(6.0);

    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Author:");
            ui.add_space(4.0);
            theme::identity_field(ui, Some(identity.display_name.as_str()));
        });
    });

    ui.add_space(8.0);
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Post:");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("{}/{} chars", state.post.chars().count(), crate::pipeline::MAX_POST_CHARS));
            });
        });
        ui.add(
            egui::TextEdit::multiline(&mut state.post)
                .char_limit(crate::pipeline::MAX_POST_CHARS)
                .desired_rows(10)
                .desired_width(f32::INFINITY),
        );
    });

    ui.add_space(8.0);
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            if ui.button("📂 Attach File...").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    match std::fs::read(&path) {
                        Ok(bytes) if bytes.len() as u64 > MAX_ATTACHMENT_BYTES => {
                            state.attachment_warning = Some(format!("{} is too large (max {})", format_bytes(bytes.len() as u64), format_bytes(MAX_ATTACHMENT_BYTES)));
                        }
                        Ok(bytes) => {
                            state.attachment_warning = None;
                            state.attachment = Some((path, bytes));
                        }
                        Err(e) => state.attachment_warning = Some(format!("couldn't read file: {e}")),
                    }
                }
            }
            match &state.attachment {
                Some((path, bytes)) => {
                    ui.label(format!("{} ({})", path.file_name().and_then(|n| n.to_str()).unwrap_or("?"), format_bytes(bytes.len() as u64)));
                    if ui.button("Remove").clicked() {
                        state.attachment = None;
                    }
                }
                None => {
                    ui.weak("No attachment (one file max, uploaded to remote storage on Post)");
                }
            }
        });
        if let Some(warning) = &state.attachment_warning {
            ui.colored_label(WARNING, format!("⚠ {warning}"));
        }
    });

    ui.add_space(10.0);

    let identity_ready = !identity.display_name.trim().is_empty() && identity.home_grid.is_some();
    let can_post =
        identity_ready && (!state.post.trim().is_empty() || state.attachment.is_some()) && matches!(state.phase, SendPhase::Idle | SendPhase::Done | SendPhase::Failed(_));

    if ui.add_enabled(can_post, egui::Button::new("Prepare Post")).clicked() {
        let author = identity.display_name.clone();
        let grid = identity.home_grid.clone();
        let post_text = state.post.clone();
        let attachment = state.attachment.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let media_url = match attachment {
                Some((path, bytes)) => {
                    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("attachment").to_string();
                    match upload_attachment(&bytes, &filename) {
                        Ok(url) => Some(url),
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    }
                }
                None => None,
            };
            let _ = tx.send(Ok(prepare_social_transmission(&author, &post_text, media_url.as_deref(), grid.as_deref())));
        });
        state.phase = SendPhase::Preparing { rx };
    }
    if !identity_ready && matches!(state.phase, SendPhase::Idle) {
        ui.colored_label(WARNING, "Set your callsign and home locator in Settings (⚙) before sending.");
    } else if !can_post && matches!(state.phase, SendPhase::Idle) {
        ui.colored_label(WARNING, "Write a post or attach a file before sending.");
    }

    ui.add_space(10.0);

    match &mut state.phase {
        SendPhase::Idle => {}
        SendPhase::Preparing { rx } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if state.attachment.is_some() { "Uploading attachment and assembling audio..." } else { "Assembling audio..." });
            });
            if let Ok(result) = rx.try_recv() {
                state.phase = match result {
                    Ok(prepared) => {
                        state.cached_wave = Some((prepared.wave.clone(), prepared.sample_rate));
                        SendPhase::Prepared { prepared }
                    }
                    Err(e) => SendPhase::Failed(e),
                };
            }
            ctx.request_repaint();
        }
        SendPhase::Prepared { prepared } => {
            let mut transmit_clicked = false;
            let mut save_error: Option<String> = None;
            let mut edit_clicked = false;
            theme::card(ui, |ui| {
                theme::status_dot(
                    ui,
                    ACCENT,
                    &format!(
                        "Ready: {} of speech at {} Hz",
                        format_duration(prepared.wave.len() as f32 / prepared.sample_rate as f32),
                        prepared.sample_rate
                    ),
                );
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("\"{}\"", prepared.spoken_text)).weak().italics());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(output_device.is_some(), egui::Button::new("📡 Post")).clicked() {
                        transmit_clicked = true;
                    }
                    if ui.button("💾 Save as WAV...").clicked() {
                        if let Some(path) =
                            rfd::FileDialog::new().set_file_name("poc_com_social_post.wav").add_filter("WAV audio", &["wav"]).save_file()
                        {
                            if let Err(e) = wav::write_wav_f32(&path, prepared.sample_rate, &prepared.wave) {
                                save_error = Some(format!("couldn't save WAV: {e}"));
                            }
                        }
                    }
                    if ui.button("Edit").on_hover_text("Go back and change the post before sending").clicked() {
                        edit_clicked = true;
                    }
                });
            });

            if transmit_clicked {
                let SendPhase::Prepared { prepared } = std::mem::replace(&mut state.phase, SendPhase::Idle) else {
                    unreachable!()
                };
                state.phase = SendPhase::Countdown { prepared, deadline: Instant::now() + PTT_LEAD_IN };
            } else if edit_clicked {
                state.phase = SendPhase::Idle;
                state.cached_wave = None;
            } else if let Some(err) = save_error {
                state.phase = SendPhase::Failed(err);
            }
        }
        SendPhase::Countdown { deadline, .. } => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            theme::card(ui, |ui| {
                let heading = if identity.ptt_port.is_some() {
                    format!("🎙 PTT will key automatically -- starting in {:.0}s", remaining.as_secs_f32().ceil())
                } else {
                    format!("🎙 Press and hold PTT now -- starting in {:.0}s", remaining.as_secs_f32().ceil())
                };
                ui.heading(heading);
            });
            if remaining.is_zero() {
                let SendPhase::Countdown { prepared, .. } =
                    std::mem::replace(&mut state.phase, SendPhase::Failed("internal state error".into()))
                else {
                    unreachable!()
                };
                let ptt = match identity.ptt_port.as_deref() {
                    Some(port) => match audio_io::PttPort::key(port, identity.ptt_baud, PTT_PRE_DELAY) {
                        Ok(p) => {
                            state.ptt_warning = None;
                            Some(p)
                        }
                        Err(e) => {
                            state.ptt_warning = Some(e.to_string());
                            None
                        }
                    },
                    None => None,
                };
                match audio_io::start_transmission(output_device.unwrap_or_default(), prepared.wave, prepared.sample_rate) {
                    Ok(tx) => state.phase = SendPhase::Transmitting { tx, ptt },
                    Err(e) => state.phase = SendPhase::Failed(e.to_string()),
                }
            }
            ctx.request_repaint();
        }
        SendPhase::Transmitting { tx, ptt } => {
            theme::card(ui, |ui| {
                let heading = if ptt.is_some() { "📡 Posting -- PTT keyed automatically" } else { "📡 Posting -- keep holding PTT" };
                ui.heading(heading);
                ui.add_space(4.0);
                ui.add(egui::ProgressBar::new(tx.progress()).show_percentage());
                if ui.button("Cancel").clicked() {
                    tx.cancel();
                }
            });
            if let Some(warning) = &state.ptt_warning {
                ui.colored_label(WARNING, format!("⚠ PTT port didn't respond, sending with manual PTT: {warning}"));
            }
            if tx.is_finished() {
                let SendPhase::Transmitting { ptt, .. } = std::mem::replace(&mut state.phase, SendPhase::Done) else { unreachable!() };
                state.phase = match ptt {
                    Some(ptt) => SendPhase::Releasing { until: Instant::now() + PTT_POST_DELAY, _ptt: Some(ptt) },
                    None => SendPhase::Done,
                };
            }
            ctx.request_repaint();
        }
        SendPhase::Releasing { until, .. } => {
            theme::card(ui, |ui| {
                theme::status_dot(ui, ACCENT, "📡 Posted -- releasing PTT...");
            });
            if Instant::now() >= *until {
                state.phase = SendPhase::Done;
            }
            ctx.request_repaint();
        }
        SendPhase::Done => {
            let mut new_post_clicked = false;
            theme::card(ui, |ui| {
                theme::status_dot(ui, theme::SUCCESS, "Posted. You can release PTT now.");
                ui.add_space(4.0);
                if ui.button("New Post").clicked() {
                    new_post_clicked = true;
                }
            });
            if new_post_clicked {
                state.reset_for_next_post();
            }
        }
        SendPhase::Failed(err) => {
            let mut try_again_clicked = false;
            theme::card(ui, |ui| {
                theme::status_dot(ui, ERROR, &format!("Failed: {err}"));
                ui.add_space(4.0);
                if ui.button("Try Again").clicked() {
                    try_again_clicked = true;
                }
            });
            if try_again_clicked {
                state.phase = SendPhase::Idle;
            }
        }
    }

    if let SendPhase::Transmitting { tx, .. } = &state.phase {
        if waterfall.wants_feed() {
            if let Some((wave, rate)) = &state.cached_wave {
                let pos = ((tx.progress() * wave.len() as f32) as usize).min(wave.len());
                let window_start = pos.saturating_sub(WaterfallWindow::FEED_WINDOW_SAMPLES);
                waterfall.feed(&wave[window_start..pos.max(window_start)], *rate);
            }
        }
    }
}

/// Video/audio/generic links open in the system browser rather than
/// rendering inline or in an embedded player. This is a deliberate safety
/// choice, not a missing feature: `media_url` comes from a decoded,
/// unauthenticated over-the-air post -- this is an open, unsigned ledger,
/// so *anyone* in range can put an arbitrary URL in someone's feed. An
/// embedded JS-capable player living inside this app's own process would
/// auto-execute whatever that URL serves; the system browser is a
/// separate, sandboxed process the user consciously navigates to instead.
/// Images are the one exception -- decoding raster bytes locally carries
/// none of that risk, so those still render inline.
enum Media {
    None,
    Video(String),
    Audio(String),
    /// Anything else with a URL (or a video/audio link the user hasn't
    /// clicked into "watch"/"listen" chrome for) -- a plain attachment link.
    Link(String),
    ImageLoading(String, mpsc::Receiver<Result<Vec<u8>, String>>),
    ImageReady(String, egui::TextureHandle),
    ImageFailed(String, String),
}

pub struct FeedPost {
    pub author: String,
    pub post: String,
    pub received_at: SystemTime,
    media: Media,
}

#[derive(Default)]
pub struct FeedState {
    pub posts: Vec<FeedPost>,
    just_received: bool,
}

impl FeedState {
    /// Called by `app.rs` after the shared listening session (see
    /// `listen.rs`) decodes a `Social` payload -- listening itself isn't
    /// mode-specific anymore, so this is the only entry point for new
    /// posts landing here regardless of which tab was on screen when they
    /// arrived. Kicks off an image fetch, or just records the URL as a
    /// video/audio/generic link.
    pub fn push_post(&mut self, author: String, post: String, media_url: Option<String>) {
        let media = match &media_url {
            Some(url) if looks_like_image(url) => spawn_image_fetch(url.clone()),
            Some(url) if looks_like_video(url) => Media::Video(url.clone()),
            Some(url) if looks_like_audio(url) => Media::Audio(url.clone()),
            Some(url) => Media::Link(url.clone()),
            None => Media::None,
        };
        self.posts.push(FeedPost { author, post, received_at: SystemTime::now(), media });
        self.just_received = true;
    }

    /// Poll any in-flight image fetches and turn newly-arrived bytes into
    /// GPU textures. Has to happen on the UI thread (`ctx.load_texture`
    /// isn't `Send`), so the background thread only does the network
    /// fetch -- decoding and texture upload happen here once bytes land.
    fn poll_image_fetches(&mut self, ctx: &egui::Context) {
        for post in self.posts.iter_mut() {
            if let Media::ImageLoading(url, rx) = &post.media {
                match rx.try_recv() {
                    Ok(Ok(bytes)) => {
                        post.media = match image::load_from_memory(&bytes) {
                            Ok(img) => {
                                let rgba = img.to_rgba8();
                                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                                let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
                                let texture = ctx.load_texture(url.clone(), color_image, egui::TextureOptions::LINEAR);
                                Media::ImageReady(url.clone(), texture)
                            }
                            Err(e) => Media::ImageFailed(url.clone(), e.to_string()),
                        };
                    }
                    Ok(Err(e)) => post.media = Media::ImageFailed(url.clone(), e),
                    Err(_) => {}
                }
            }
        }
    }
}

/// Kicks off a background fetch for an image URL, returning the
/// `Loading` state to store immediately. Shared by the initial fetch (when
/// a post first decodes) and by the feed's "Try again" retry action --
/// remote blob storage is a best-effort service (litterbox.moe has been
/// observed to 504 transiently), so a failed fetch shouldn't be a dead end.
fn spawn_image_fetch(url: String) -> Media {
    let (tx, rx) = mpsc::channel();
    let url_clone = url.clone();
    std::thread::spawn(move || {
        let _ = tx.send(fetch_blob(&url_clone));
    });
    Media::ImageLoading(url, rx)
}

fn url_extension(url: &str) -> String {
    url.split('.').next_back().unwrap_or("").split('?').next().unwrap_or("").to_lowercase()
}

fn looks_like_image(url: &str) -> bool {
    matches!(url_extension(url).as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "bmp")
}

fn looks_like_video(url: &str) -> bool {
    matches!(url_extension(url).as_str(), "mp4" | "webm" | "mov" | "mkv" | "avi")
}

fn looks_like_audio(url: &str) -> bool {
    matches!(url_extension(url).as_str(), "mp3" | "wav" | "ogg" | "flac" | "m4a" | "opus" | "aac")
}

pub fn feed_ui(ui: &mut egui::Ui, ctx: &egui::Context, state: &mut FeedState) {
    state.poll_image_fetches(ctx);

    ui.heading("Feed");
    ui.add_space(6.0);

    // Chat-board convention: oldest at the top, newest at the bottom,
    // sticking to the bottom as new posts arrive -- unlike POC-COM_MAIL's
    // newest-first inbox, a board reads like a conversation.
    egui::ScrollArea::vertical().id_salt("board_feed").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if state.posts.is_empty() {
            ui.weak("No posts received yet.");
        }
        for post in state.posts.iter_mut() {
            let mut retry_url: Option<String> = None;
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(display_or_dash(&post.author)).strong());
                    ui.weak(format!("· {}", format_hms(post.received_at)));
                });
                if !post.post.is_empty() {
                    ui.add_space(2.0);
                    ui.label(&post.post);
                }
                match &post.media {
                    Media::None => {}
                    Media::Video(url) => {
                        ui.add_space(4.0);
                        if ui.link("🎬 Watch video (opens in browser)").on_hover_text(url.as_str()).clicked() {
                            let _ = webbrowser::open(url);
                        }
                    }
                    Media::Audio(url) => {
                        ui.add_space(4.0);
                        if ui.link("🎵 Play audio (opens in browser)").on_hover_text(url.as_str()).clicked() {
                            let _ = webbrowser::open(url);
                        }
                    }
                    Media::Link(url) => {
                        ui.add_space(4.0);
                        if ui.link(format!("📎 {url}")).clicked() {
                            let _ = webbrowser::open(url);
                        }
                    }
                    Media::ImageLoading(_, _) => {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("Loading image...");
                        });
                    }
                    Media::ImageReady(url, texture) => {
                        ui.add_space(4.0);
                        let max_w = ui.available_width().min(360.0);
                        let scale = (max_w / texture.size()[0] as f32).min(1.0);
                        let size = egui::vec2(texture.size()[0] as f32 * scale, texture.size()[1] as f32 * scale);
                        let resp = ui.add(egui::ImageButton::new((texture.id(), size)).frame(false)).on_hover_text(format!("Open {url}"));
                        if resp.clicked() {
                            let _ = webbrowser::open(url);
                        }
                    }
                    Media::ImageFailed(url, err) => {
                        ui.add_space(4.0);
                        ui.colored_label(ERROR, format!("Image failed to load ({err})"));
                        ui.horizontal(|ui| {
                            if ui.link(format!("📎 {url}")).clicked() {
                                let _ = webbrowser::open(url);
                            }
                            // Remote blob storage is best-effort (litterbox.moe
                            // has been observed to 504 transiently) -- a failed
                            // fetch shouldn't be a dead end when the file is
                            // very likely still there.
                            if ui.button("🔄 Try again").clicked() {
                                retry_url = Some(url.clone());
                            }
                        });
                    }
                }
            });
            if let Some(url) = retry_url {
                post.media = spawn_image_fetch(url);
            }
            ui.add_space(6.0);
        }
    });
}

fn display_or_dash(s: &str) -> &str {
    if s.trim().is_empty() {
        "Anonymous"
    } else {
        s
    }
}

fn format_hms(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02} UTC")
}

fn format_duration(seconds: f32) -> String {
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{:.0}m{:02.0}s", (seconds / 60.0).floor(), seconds % 60.0)
    }
}
