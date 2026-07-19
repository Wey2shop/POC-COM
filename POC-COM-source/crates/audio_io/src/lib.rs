//! cpal-backed audio device enumeration and streaming TX/RX.
//!
//! Deliberately narrow surface: no gain/volume parameter exists anywhere
//! in this crate's public API, and TX (`tx::start_transmission`) and RX
//! (`rx::start_reception`) are structurally separate -- there is no code
//! path that reads from an input stream and writes to an output stream,
//! so passthrough/monitoring isn't just discouraged by convention, it's
//! not wired up at all.

mod device;
mod error;
mod ptt;
mod resample;
mod rx;
mod tx;

pub use device::{list_input_devices, list_output_devices, DeviceInfo};
pub use error::AudioIoError;
pub use ptt::{list_ports as list_ptt_ports, PttPort, BAUD_RATES as PTT_BAUD_RATES, DEFAULT_BAUD as PTT_DEFAULT_BAUD};
pub use resample::resample;
pub use rx::{start_reception, RxHandle};
pub use tx::{start_transmission, TxHandle};
