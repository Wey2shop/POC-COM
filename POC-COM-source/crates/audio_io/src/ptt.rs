//! Optional serial-port PTT keying, entirely separate from the audio
//! TX/RX path in `tx.rs`/`rx.rs`. If no port is configured, nothing in
//! this module is ever touched and PTT stays exactly what it's always
//! been: the user keying the radio by hand during the on-screen
//! countdown.
//!
//! Keys PTT by raising the RTS and DTR hardware control lines -- the
//! actual mechanism most simple USB PTT interfaces use (confirmed
//! against this project's own tested USB2RIG hardware, and against its
//! working MMSSTV setup, which has "DTR/RTS: PTT" checked rather than
//! relying on CAT command text alone) -- and also sends the Kenwood
//! `TX;`/`RX;` CAT command text (the same protocol MMSSTV's "Kenwood,
//! Elecraft" preset uses), which is harmless for a pure RTS/DTR
//! interface and still covers a genuine CAT-capable rig on the same
//! port.

use crate::error::AudioIoError;
use std::io::Write;
use std::time::Duration;

/// Baud rates offered in the Settings picker -- covers the range real
/// rig/interface CAT ports actually use. `DEFAULT_BAUD` (4800, 8 data
/// bits, 2 stop bits, no parity) matches this project's own tested
/// USB2RIG hardware and MMSSTV's built-in "Kenwood, Elecraft" preset;
/// data bits/stop bits/parity aren't exposed since those are fixed by
/// the same preset and rarely need changing independently of baud.
pub const BAUD_RATES: [u32; 8] = [1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200];
pub const DEFAULT_BAUD: u32 = 4800;

/// List available serial port names for the Settings picker.
pub fn list_ports() -> Vec<String> {
    serialport::available_ports().map(|ports| ports.into_iter().map(|p| p.port_name).collect()).unwrap_or_default()
}

/// An open, keyed serial PTT connection -- opened right when a
/// transmission starts (see `key`) and dropped right when it ends,
/// never held open any longer than that.
pub struct PttPort {
    port: Box<dyn serialport::SerialPort>,
}

impl PttPort {
    /// Opens `port_name`, raises the RTS/DTR hardware control lines to
    /// key PTT, and waits `settle` for the radio's relay to physically
    /// close before audio starts -- mirrors MMSSTV's own `TX;\w10`
    /// command template (its `\w10` is MMSSTV's own 10ms post-command
    /// wait directive, not literal bytes sent over the wire).
    ///
    /// RTS/DTR, not the `TX;` text, is the actual key-up signal for most
    /// simple USB PTT interfaces (including this project's own tested
    /// USB2RIG hardware) -- PTT is wired straight to those hardware
    /// control lines, not parsed from anything sent over the data pins.
    /// Confirmed against this project's own working MMSSTV setup, which
    /// has its "DTR/RTS: PTT" checkbox enabled rather than relying on the
    /// CAT command text alone; sending the Kenwood `TX;`/`RX;` text too
    /// is harmless for a pure RTS/DTR interface (nothing is listening for
    /// it) and still covers a genuine CAT-capable rig on the same port.
    pub fn key(port_name: &str, baud: u32, settle: Duration) -> Result<Self, AudioIoError> {
        let mut port = serialport::new(port_name, baud)
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::Two)
            .parity(serialport::Parity::None)
            .timeout(Duration::from_millis(500))
            .open()
            .map_err(|e| AudioIoError::Ptt(format!("couldn't open {port_name}: {e}")))?;
        port.write_request_to_send(true).map_err(|e| AudioIoError::Ptt(format!("RTS key-up on {port_name} failed: {e}")))?;
        let _ = port.write_data_terminal_ready(true);
        let _ = port.write_all(b"TX;");
        let _ = port.flush();
        std::thread::sleep(settle);
        Ok(Self { port })
    }
}

impl Drop for PttPort {
    /// Always drops RTS/DTR and sends the Kenwood `RX;` key-down command
    /// on drop -- keyed PTT must never be left on because of an error
    /// partway through a transmission or an early cancel, so this runs
    /// unconditionally rather than only on a "happy path" a caller would
    /// have to remember to call explicitly.
    fn drop(&mut self) {
        let _ = self.port.write_request_to_send(false);
        let _ = self.port.write_data_terminal_ready(false);
        let _ = self.port.write_all(b"RX;");
        let _ = self.port.flush();
    }
}
