//! System audio device enumeration (inputs = mics, outputs = speakers/headphones).
//!
//! Uses cpal on macOS (CoreAudio) and Windows (WASAPI), and PipeWire on Linux
//! (where ALSA, under cpal, lists plugin names like `sysdefault:CARD=…`).
//! Device ids are stable across reboots and replugging (indices aren't): the
//! device *names* on macOS and Windows, PipeWire's `node.name` on Linux.

#[cfg(not(target_os = "linux"))]
use cpal::traits::{DeviceTrait, HostTrait};

use crate::Device;

/// Sentinel device id meaning "follow the OS default device".
pub const DEFAULT_DEVICE: &str = "default";

/// Audio devices on this machine, plus which ones the OS currently uses by default.
#[derive(Debug, Clone, Default)]
pub struct AudioDevices {
    pub inputs: Vec<Device>,
    pub outputs: Vec<Device>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

impl AudioDevices {
    /// Resolve a device id (possibly `DEFAULT_DEVICE`) to a concrete device name.
    pub fn resolve_input(&self, id: &str) -> Option<String> {
        resolve(id, self.default_input.as_deref())
    }
    pub fn resolve_output(&self, id: &str) -> Option<String> {
        resolve(id, self.default_output.as_deref())
    }
}

fn resolve(id: &str, default: Option<&str>) -> Option<String> {
    if id == DEFAULT_DEVICE {
        default.map(str::to_owned)
    } else {
        Some(id.to_owned())
    }
}

/// Enumerate input and output devices. Never fails — a broken audio host just
/// yields empty lists.
#[cfg(target_os = "linux")]
pub fn list_audio_devices() -> AudioDevices {
    crate::linux::audio::list_devices()
}

/// Enumerate input and output devices. Never fails — a broken audio host just
/// yields empty lists.
#[cfg(not(target_os = "linux"))]
pub fn list_audio_devices() -> AudioDevices {
    let host = cpal::default_host();
    let name = |d: &cpal::Device| d.description().ok().map(|desc| desc.name().to_owned());
    let to_devices = |devs: Option<Vec<cpal::Device>>| -> Vec<Device> {
        devs.unwrap_or_default()
            .iter()
            .filter_map(name)
            .map(|n| Device { id: n.clone(), name: n })
            .collect()
    };
    AudioDevices {
        inputs: to_devices(host.input_devices().ok().map(Iterator::collect)),
        outputs: to_devices(host.output_devices().ok().map(Iterator::collect)),
        default_input: host.default_input_device().as_ref().and_then(name),
        default_output: host.default_output_device().as_ref().and_then(name),
    }
}
