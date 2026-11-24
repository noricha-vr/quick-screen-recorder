use std::sync::mpsc;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDevicePosition, AVCaptureDeviceTypeExternal, AVCaptureDeviceTypeMicrophone,
    AVMediaTypeAudio,
};
use objc2_foundation::NSArray;

use crate::capture::MicInputPreference;
use crate::fs_safety::sanitize_terminal_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPermissionStatus {
    Authorized,
    NotDetermined,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicMenuItem {
    Off,
    SystemDefault { label: String },
    Device { id: String, name: String },
}

pub fn microphone_permission_status() -> MicPermissionStatus {
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        return MicPermissionStatus::Denied;
    };
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    if status == AVAuthorizationStatus::Authorized {
        MicPermissionStatus::Authorized
    } else if status == AVAuthorizationStatus::NotDetermined {
        MicPermissionStatus::NotDetermined
    } else {
        MicPermissionStatus::Denied
    }
}

/// Triggers the native macOS microphone permission prompt asynchronously without blocking the main thread.
pub fn request_microphone_permission_channel() -> mpsc::Receiver<bool> {
    let (tx, rx) = mpsc::sync_channel::<bool>(1);
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        let _ = tx.send(false);
        return rx;
    };
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &block);
    }
    rx
}

/// Lists available macOS microphone input devices, marking the system default device first.
pub fn list_microphone_devices() -> Vec<MicDeviceInfo> {
    let Some(audio_type) = (unsafe { AVMediaTypeAudio }) else {
        return Vec::new();
    };
    let default_id = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(audio_type) }
        .map(|d| unsafe { d.uniqueID().to_string() });

    let types = unsafe {
        NSArray::from_slice(&[AVCaptureDeviceTypeMicrophone, AVCaptureDeviceTypeExternal])
    };
    let session = unsafe {
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &types,
            Some(audio_type),
            AVCaptureDevicePosition::Unspecified,
        )
    };
    let devices = unsafe { session.devices() };
    let mut out = Vec::with_capacity(devices.count());
    for i in 0..devices.count() {
        let dev = devices.objectAtIndex(i);
        let id = unsafe { dev.uniqueID().to_string() };
        if id.is_empty() || out.iter().any(|existing: &MicDeviceInfo| existing.id == id) {
            continue;
        }
        let raw_name = unsafe { dev.localizedName().to_string() };
        let name = sanitize_terminal_text(raw_name.trim(), false);
        let display = if name.is_empty() {
            "Microphone".to_string()
        } else {
            name
        };
        let is_default = default_id.as_deref() == Some(id.as_str());
        out.push(MicDeviceInfo {
            id,
            name: display,
            is_default,
        });
    }
    out.sort_by_key(|d| (!d.is_default, d.name.to_ascii_lowercase()));
    out
}

/// Builds the microphone dropdown model (`[Off, System Default (...), <individual devices...>]`)
/// and returns `(items, selected_index)`. If a previously saved device is disconnected, falls back to `SystemDefault`.
pub fn build_mic_menu_model(
    pref: &MicInputPreference,
    devices: &[MicDeviceInfo],
) -> (Vec<MicMenuItem>, usize) {
    let default_label = match devices
        .iter()
        .find(|d| d.is_default)
        .or_else(|| devices.first())
    {
        Some(d) => format!("System ({})", truncate_label(&d.name, 18)),
        None => "System Default".to_string(),
    };
    let mut items = Vec::with_capacity(devices.len() + 2);
    items.push(MicMenuItem::Off);
    items.push(MicMenuItem::SystemDefault {
        label: default_label,
    });
    for dev in devices {
        items.push(MicMenuItem::Device {
            id: dev.id.clone(),
            name: truncate_label(&dev.name, 26),
        });
    }

    let selected_idx = match pref {
        MicInputPreference::Off => 0,
        MicInputPreference::SystemDefault => 1,
        MicInputPreference::Device { id, .. } => items
            .iter()
            .position(|it| matches!(it, MicMenuItem::Device { id: d, .. } if d == id))
            .unwrap_or(1),
    };
    (items, selected_idx)
}

impl MicMenuItem {
    pub fn menu_title(&self) -> String {
        match self {
            Self::Off => "Off".to_string(),
            Self::SystemDefault { label } => label.clone(),
            Self::Device { name, .. } => format!("Mic: {name}"),
        }
    }

    pub fn to_preference(&self) -> MicInputPreference {
        match self {
            Self::Off => MicInputPreference::Off,
            Self::SystemDefault { .. } => MicInputPreference::SystemDefault,
            Self::Device { id, name } => MicInputPreference::Device {
                id: id.clone(),
                name: name.clone(),
            },
        }
    }
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1).max(3);
    let head: String = chars[..keep].iter().collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_menu_model_selects_saved_device_and_falls_back_when_unplugged() {
        let devs = vec![
            MicDeviceInfo {
                id: "builtin-1".into(),
                name: "Built-in Microphone".into(),
                is_default: true,
            },
            MicDeviceInfo {
                id: "usb-2".into(),
                name: "Studio USB Mic".into(),
                is_default: false,
            },
        ];

        let (items, idx_default) = build_mic_menu_model(&MicInputPreference::SystemDefault, &devs);
        assert_eq!(idx_default, 1);
        assert_eq!(items[0], MicMenuItem::Off);
        assert!(items[1].menu_title().starts_with("System ("));

        let (_, idx_usb) = build_mic_menu_model(
            &MicInputPreference::Device {
                id: "usb-2".into(),
                name: "Studio USB Mic".into(),
            },
            &devs,
        );
        assert_eq!(idx_usb, 3);

        // Unplugged device falls back cleanly to SystemDefault (index 1)
        let (_, idx_missing) = build_mic_menu_model(
            &MicInputPreference::Device {
                id: "removed-9".into(),
                name: "Old Headset".into(),
            },
            &devs,
        );
        assert_eq!(idx_missing, 1);

        let (_, idx_off) = build_mic_menu_model(&MicInputPreference::Off, &devs);
        assert_eq!(idx_off, 0);
    }

    #[test]
    fn mic_menu_model_keeps_duplicate_and_reserved_names_distinct_by_id() {
        let devs = vec![
            MicDeviceInfo {
                id: "dev-a".into(),
                name: "Off".into(),
                is_default: true,
            },
            MicDeviceInfo {
                id: "dev-b".into(),
                name: "Off".into(),
                is_default: false,
            },
        ];
        let (items, idx_b) = build_mic_menu_model(
            &MicInputPreference::Device {
                id: "dev-b".into(),
                name: "Off".into(),
            },
            &devs,
        );
        assert_eq!(items.len(), 4);
        assert_eq!(items[0], MicMenuItem::Off);
        assert_eq!(items[1].menu_title(), "System (Off)");
        assert_eq!(items[2].menu_title(), "Mic: Off");
        assert_ne!(items[0].menu_title(), items[2].menu_title());
        assert_eq!(idx_b, 3);
        assert_eq!(
            items[idx_b].to_preference(),
            MicInputPreference::Device {
                id: "dev-b".into(),
                name: "Off".into(),
            }
        );
    }
}
