//! Shared identity (display name/callsign + home Maidenhead locator), set
//! once via the Settings popover and reused everywhere Mail's `From`,
//! Mail's `Location`, and Social's `Author` used to be typed separately.
//! Also holds the optional serial PTT port (see `audio_io::ptt`) -- most
//! users never touch it, so it stays off (`None`) unless explicitly
//! picked in Settings.
//!
//! Persisted as a plain line-oriented text file under `%APPDATA%` rather
//! than pulling in a serialization crate -- this is the only piece of
//! state in the whole app worth surviving a restart (nobody wants to
//! retype their callsign, re-pick their grid square, or re-select their
//! PTT port every launch), and a few lines of text don't need `serde`.

use std::path::PathBuf;

pub struct Identity {
    pub display_name: String,
    pub home_grid: Option<String>,
    pub ptt_port: Option<String>,
    pub ptt_baud: u32,
}

impl Default for Identity {
    fn default() -> Self {
        Self { display_name: String::new(), home_grid: None, ptt_port: None, ptt_baud: audio_io::PTT_DEFAULT_BAUD }
    }
}

fn settings_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("POC-COM").join("identity.txt"))
}

pub fn load() -> Identity {
    let Some(path) = settings_path() else {
        return Identity::default();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Identity::default();
    };
    let mut lines = contents.lines();
    let display_name = lines.next().unwrap_or_default().to_string();
    let home_grid = lines.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    // Older saved files predate the PTT port/baud lines -- a missing line
    // just means "no PTT configured" / "default baud", same as an empty
    // or unparseable one.
    let ptt_port = lines.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let ptt_baud = lines.next().and_then(|s| s.trim().parse().ok()).unwrap_or(audio_io::PTT_DEFAULT_BAUD);
    Identity { display_name, home_grid, ptt_port, ptt_baud }
}

/// Best-effort write -- there's nothing the caller can usefully do about a
/// failed save here (no writable `%APPDATA%`, disk full, etc.), so this
/// silently no-ops on error rather than surfacing a save-settings error UI
/// for what's ultimately a convenience feature.
pub fn save(identity: &Identity) {
    let Some(path) = settings_path() else { return };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let contents = format!(
        "{}\n{}\n{}\n{}\n",
        identity.display_name,
        identity.home_grid.as_deref().unwrap_or(""),
        identity.ptt_port.as_deref().unwrap_or(""),
        identity.ptt_baud
    );
    let _ = std::fs::write(&path, contents);
}
