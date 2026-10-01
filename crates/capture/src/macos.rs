//! Native macOS capture via ScreenCaptureKit.
//!
//! First milestone: enumerate shareable content (displays + running apps) — this
//! proves the objc2 SCK binding works and feeds the per-app audio picker. The
//! SCStream → AVAssetWriter recording pipeline builds on top of this.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::SCShareableContent;

use crate::{Device, Devices};

/// Whether this process currently has macOS Screen Recording permission.
/// Cheap, non-prompting — safe to call every frame.
pub fn screen_recording_authorized() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Trigger the system Screen Recording permission prompt (first run only; afterwards
/// macOS just returns the current status and the user must use System Settings).
/// Returns whether access is granted.
pub fn request_screen_recording() -> bool {
    CGRequestScreenCaptureAccess()
}

/// Ask ScreenCaptureKit for the displays and applications available to capture.
///
/// Requires Screen Recording permission for real results; without it macOS returns
/// an error and this yields empty lists (not a crash).
pub fn list_shareable() -> Devices {
    let (tx, rx) = mpsc::channel::<Devices>();

    // Completion handler fires on a background dispatch queue. We pull everything
    // into owned Rust data there and send it back over the channel.
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, _err: *mut NSError| {
            let mut devices = Devices::default();
            if !content.is_null() {
                let content: &SCShareableContent = unsafe { &*content };
                unsafe {
                    for display in content.displays().iter() {
                        let id = display.displayID();
                        let (w, h) = (display.width(), display.height());
                        devices.screens.push(Device {
                            id: id.to_string(),
                            name: format!("Display {id} ({w}×{h})"),
                        });
                    }
                    for app in content.applications().iter() {
                        let name = app.applicationName().to_string();
                        if !name.is_empty() {
                            devices.apps.push(Device {
                                id: app.bundleIdentifier().to_string(),
                                name,
                            });
                        }
                    }
                }
            }
            let _ = tx.send(devices);
        },
    );

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&handler);
    }

    rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
}

/// Apps the user could add as an audio source: running apps with a Dock icon
/// (no background agents or system services), sorted by name.
pub fn list_apps() -> Vec<Device> {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
    let mut apps: Vec<Device> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .filter_map(|a| {
            let id = a.bundleIdentifier()?.to_string();
            let name = a.localizedName().map_or_else(|| id.clone(), |n| n.to_string());
            Some(Device { id, name })
        })
        .filter(|d| d.id != "com.apple.finder")
        .collect();
    apps.sort_by_key(|d| d.name.to_lowercase());
    apps.dedup_by(|a, b| a.id == b.id);
    apps
}
