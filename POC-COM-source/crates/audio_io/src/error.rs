#[derive(Debug)]
pub enum AudioIoError {
    NoHostDevices(String),
    DeviceNotFound(String),
    UnsupportedConfig(String),
    StreamBuild(String),
    StreamPlay(String),
    Ptt(String),
}

impl std::fmt::Display for AudioIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHostDevices(s) => write!(f, "could not enumerate audio devices: {s}"),
            Self::DeviceNotFound(name) => write!(f, "audio device not found: {name}"),
            Self::UnsupportedConfig(s) => write!(f, "unsupported device audio config: {s}"),
            Self::StreamBuild(s) => write!(f, "failed to build audio stream: {s}"),
            Self::StreamPlay(s) => write!(f, "failed to start audio stream: {s}"),
            Self::Ptt(s) => write!(f, "PTT serial port error: {s}"),
        }
    }
}

impl std::error::Error for AudioIoError {}
