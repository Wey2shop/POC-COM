use crate::error::AudioIoError;
use cpal::traits::{DeviceTrait, HostTrait};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
}

/// Enumerate input devices for the GUI's device picker. Names are used as
/// the stable identifier passed back into `start_transmission` /
/// `start_reception` -- cpal names are stable per physical device within a
/// session, which is good enough for a device picker dropdown.
pub fn list_input_devices() -> Result<Vec<DeviceInfo>, AudioIoError> {
    let host = cpal::default_host();
    let devices = host.input_devices().map_err(|e| AudioIoError::NoHostDevices(e.to_string()))?;
    Ok(devices.filter_map(|d| d.name().ok()).map(|name| DeviceInfo { name }).collect())
}

pub fn list_output_devices() -> Result<Vec<DeviceInfo>, AudioIoError> {
    let host = cpal::default_host();
    let devices = host.output_devices().map_err(|e| AudioIoError::NoHostDevices(e.to_string()))?;
    Ok(devices.filter_map(|d| d.name().ok()).map(|name| DeviceInfo { name }).collect())
}

pub(crate) fn find_output_device(name: &str) -> Result<cpal::Device, AudioIoError> {
    let host = cpal::default_host();
    let devices = host.output_devices().map_err(|e| AudioIoError::NoHostDevices(e.to_string()))?;
    devices
        .into_iter()
        .find(|d| d.name().map(|n| n == name).unwrap_or(false))
        .ok_or_else(|| AudioIoError::DeviceNotFound(name.to_string()))
}

pub(crate) fn find_input_device(name: &str) -> Result<cpal::Device, AudioIoError> {
    let host = cpal::default_host();
    let devices = host.input_devices().map_err(|e| AudioIoError::NoHostDevices(e.to_string()))?;
    devices
        .into_iter()
        .find(|d| d.name().map(|n| n == name).unwrap_or(false))
        .ok_or_else(|| AudioIoError::DeviceNotFound(name.to_string()))
}
