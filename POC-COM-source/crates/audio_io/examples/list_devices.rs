//! Quick manual sanity check for device enumeration: `cargo run --example list_devices -p audio_io`
fn main() {
    match audio_io::list_input_devices() {
        Ok(devices) => {
            println!("Input devices ({}):", devices.len());
            for d in devices {
                println!("  - {}", d.name);
            }
        }
        Err(e) => println!("Failed to enumerate input devices: {e}"),
    }

    match audio_io::list_output_devices() {
        Ok(devices) => {
            println!("Output devices ({}):", devices.len());
            for d in devices {
                println!("  - {}", d.name);
            }
        }
        Err(e) => println!("Failed to enumerate output devices: {e}"),
    }
}
