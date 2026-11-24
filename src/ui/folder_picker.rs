use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAlert, NSApplication, NSColor, NSMenuItem, NSModalResponseOK, NSOpenPanel, NSPopUpButton,
    NSTextField,
};
use objc2_foundation::{NSString, NSURL};

use crate::fs_safety::{
    default_movies_export_dir, format_tilde_path, is_usable_export_dir, next_unique_token,
    sanitize_terminal_text, valid_home_dir,
};
use crate::ui::editor_worker::EditorState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderMenuItem {
    Dir {
        path: PathBuf,
        label: String,
        selected: bool,
    },
    Placeholder,
    Separator,
    ChooseCustom,
}

/// Returns verified, canonicalized standard macOS export folders owned by the current user,
/// starting with `~/Movies/Quick Screen Recorder`.
pub fn preset_export_dirs() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(5);
    if let Some(app_movies) = default_movies_export_dir() {
        let canon = fs::canonicalize(&app_movies).unwrap_or(app_movies);
        if is_usable_export_dir(&canon) && !out.contains(&canon) {
            out.push(canon);
        }
    }
    let Some(home) = valid_home_dir() else {
        return out;
    };
    for sub in ["Movies", "Desktop", "Downloads", "Documents"] {
        let cand = home.join(sub);
        if is_usable_export_dir(&cand) {
            let canon = fs::canonicalize(&cand).unwrap_or(cand);
            if is_usable_export_dir(&canon) && !out.contains(&canon) {
                out.push(canon);
            }
        }
    }
    out
}

/// Formats a folder path using `~` and middle-truncates long paths while preserving `~/` and the leaf folder name.
pub fn compact_tilde_folder(dir: &Path, max_chars: usize) -> String {
    let tilde = format_tilde_path(dir);
    let chars: Vec<char> = tilde.chars().collect();
    if chars.len() <= max_chars {
        return tilde;
    }
    if let Some(rest) = tilde.strip_prefix("~/") {
        if let Some((_, leaf)) = rest.rsplit_once('/') {
            let candidate = format!("~/…/{leaf}");
            if candidate.chars().count() <= max_chars {
                return candidate;
            }
        }
    }
    let keep = max_chars.saturating_sub(1).max(4);
    let tail: String = chars[chars.len().saturating_sub(keep)..].iter().collect();
    format!("…{tail}")
}

/// Pure menu-model builder used by both the Editor `NSPopUpButton` and Menu Bar `Save Folder` submenu.
pub fn build_folder_menu_model(
    current: Option<&Path>,
    presets: &[PathBuf],
    max_chars: usize,
) -> (Vec<FolderMenuItem>, usize) {
    let mut items = Vec::with_capacity(presets.len() + 5);
    let mut selected_idx = 0_usize;

    match current {
        Some(cur) => {
            let in_presets = presets.iter().any(|p| p == cur);
            if !in_presets {
                items.push(FolderMenuItem::Dir {
                    path: cur.to_path_buf(),
                    label: compact_tilde_folder(cur, max_chars),
                    selected: true,
                });
                selected_idx = 0;
                if !presets.is_empty() {
                    items.push(FolderMenuItem::Separator);
                }
            }
            for preset in presets {
                let is_sel = preset == cur;
                if is_sel {
                    selected_idx = items.len();
                }
                items.push(FolderMenuItem::Dir {
                    path: preset.clone(),
                    label: compact_tilde_folder(preset, max_chars),
                    selected: is_sel,
                });
            }
        }
        None => {
            items.push(FolderMenuItem::Placeholder);
            selected_idx = 0;
            if !presets.is_empty() {
                items.push(FolderMenuItem::Separator);
                for preset in presets {
                    items.push(FolderMenuItem::Dir {
                        path: preset.clone(),
                        label: compact_tilde_folder(preset, max_chars),
                        selected: false,
                    });
                }
            }
        }
    }

    items.push(FolderMenuItem::Separator);
    items.push(FolderMenuItem::ChooseCustom);
    (items, selected_idx)
}

pub fn live_folder_menu_model(
    current: Option<&Path>,
    demo_showcase: bool,
    max_chars: usize,
) -> (Vec<FolderMenuItem>, usize) {
    let presets = preset_export_dirs();
    let valid_cur = current
        .filter(|d| is_usable_export_dir(d))
        .map(|d| fs::canonicalize(d).unwrap_or_else(|_| d.to_path_buf()))
        .filter(|d| is_usable_export_dir(d));

    if demo_showcase && valid_cur.is_none() && presets.is_empty() {
        return (
            vec![
                FolderMenuItem::Dir {
                    path: PathBuf::from("~/Movies/Quick Screen Recorder"),
                    label: "~/Movies/Quick Screen Recorder".to_string(),
                    selected: true,
                },
                FolderMenuItem::Separator,
                FolderMenuItem::ChooseCustom,
            ],
            0,
        );
    }

    build_folder_menu_model(valid_cur.as_deref(), &presets, max_chars)
}

pub fn populate_folder_popup(
    mtm: MainThreadMarker,
    popup: &NSPopUpButton,
    current: Option<&Path>,
    demo_showcase: bool,
) -> Vec<FolderMenuItem> {
    popup.removeAllItems();
    let (items, selected_idx) = live_folder_menu_model(current, demo_showcase, 34);

    if let Some(menu) = popup.menu() {
        menu.setAutoenablesItems(false);
        for (idx, item) in items.iter().enumerate() {
            match item {
                FolderMenuItem::Dir { label, .. } => {
                    let placeholder = format!("\u{1f}qsr-folder-{}-{idx}", next_unique_token());
                    let safe_label = sanitize_terminal_text(label, false);
                    popup.addItemWithTitle(&NSString::from_str(&placeholder));
                    if let Some(mi) = popup.lastItem() {
                        mi.setTitle(&NSString::from_str(&safe_label));
                        mi.setEnabled(true);
                    }
                }
                FolderMenuItem::Placeholder => {
                    let placeholder = format!("\u{1f}qsr-folder-{}-{idx}", next_unique_token());
                    popup.addItemWithTitle(&NSString::from_str(&placeholder));
                    if let Some(mi) = popup.lastItem() {
                        mi.setTitle(&NSString::from_str("Select Folder..."));
                        mi.setEnabled(false);
                    }
                }
                FolderMenuItem::Separator => {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                }
                FolderMenuItem::ChooseCustom => {
                    let placeholder = format!("\u{1f}qsr-folder-{}-{idx}", next_unique_token());
                    popup.addItemWithTitle(&NSString::from_str(&placeholder));
                    if let Some(mi) = popup.lastItem() {
                        mi.setTitle(&NSString::from_str("Choose Folder..."));
                        mi.setEnabled(true);
                    }
                }
            }
        }
    }

    popup.selectItemAtIndex(selected_idx as isize);
    let tip = current
        .filter(|d| is_usable_export_dir(d))
        .map(format_tilde_path);
    popup.setToolTip(tip.as_deref().map(NSString::from_str).as_deref());
    items
}

pub fn reselect_active_folder_item(popup: &NSPopUpButton, items: &[FolderMenuItem]) {
    let idx = items
        .iter()
        .position(|it| {
            matches!(
                it,
                FolderMenuItem::Dir { selected: true, .. } | FolderMenuItem::Placeholder
            )
        })
        .unwrap_or(0);
    popup.selectItemAtIndex(idx as isize);
}

pub fn choose_export_directory(
    mtm: MainThreadMarker,
    current_dir: Option<&Path>,
) -> Option<Result<PathBuf, &'static str>> {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setCanCreateDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setTitle(Some(&NSString::from_str("Choose Export Folder")));
    panel.setMessage(Some(&NSString::from_str(
        "Select or create the folder where recordings will be saved.",
    )));
    panel.setPrompt(Some(&NSString::from_str("Select Folder")));

    let seed_dir = current_dir
        .filter(|d| d.is_dir())
        .map(Path::to_path_buf)
        .or_else(valid_home_dir);
    if let Some(s) = seed_dir.as_deref().and_then(Path::to_str) {
        let url = NSURL::fileURLWithPath(&NSString::from_str(s));
        panel.setDirectoryURL(Some(&url));
    }

    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    let picked_raw = panel
        .URL()
        .and_then(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()))?;
    if !is_usable_export_dir(&picked_raw) {
        return Some(Err(
            "Selected destination must be a writable folder owned by your user (not a symlink)",
        ));
    }
    let picked = fs::canonicalize(&picked_raw).unwrap_or(picked_raw);
    if is_usable_export_dir(&picked) {
        Some(Ok(picked))
    } else {
        Some(Err("Selected destination is not a valid folder"))
    }
}

pub fn choose_video_file(mtm: MainThreadMarker) -> Option<PathBuf> {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    panel
        .URL()
        .and_then(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()))
}

pub fn show_error_alert(mtm: MainThreadMarker, title: &str, message: &str) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.runModal();
}

#[allow(clippy::too_many_arguments)]
pub fn handle_editor_folder_popup_selection(
    mtm: MainThreadMarker,
    idx: isize,
    state: &RefCell<EditorState>,
    popup: &NSPopUpButton,
    items: &RefCell<Vec<FolderMenuItem>>,
    status_label: &NSTextField,
    demo_showcase: bool,
    on_export_dir_change: &dyn Fn(PathBuf),
) {
    let (busy, cur_dir) = {
        let st = state.borrow();
        (st.busy, st.export_dir.clone())
    };
    let chosen_item = items.borrow().get(idx.max(0) as usize).cloned();
    reselect_active_folder_item(popup, &items.borrow());
    if busy {
        return;
    }
    let next_dir = match chosen_item {
        Some(FolderMenuItem::Dir { path, .. }) => {
            if !is_usable_export_dir(&path) {
                status_label.setTextColor(Some(&NSColor::systemRedColor()));
                status_label.setStringValue(&NSString::from_str(
                    "Selected folder is unavailable or unsafe; pick another folder",
                ));
                return;
            }
            Some(path)
        }
        Some(FolderMenuItem::ChooseCustom) => {
            match choose_export_directory(mtm, cur_dir.as_deref()) {
                None => None,
                Some(Err(msg)) => {
                    status_label.setTextColor(Some(&NSColor::systemRedColor()));
                    status_label.setStringValue(&NSString::from_str(msg));
                    None
                }
                Some(Ok(picked)) => Some(picked),
            }
        }
        _ => None,
    };
    if let Some(picked) = next_dir {
        state.borrow_mut().export_dir = Some(picked.clone());
        let refreshed = populate_folder_popup(mtm, popup, Some(&picked), demo_showcase);
        *items.borrow_mut() = refreshed;
        status_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.68, 0.74, 0.84, 1.0,
        )));
        status_label.setStringValue(&NSString::from_str(&format!(
            "Ready · Saves to {}",
            format_tilde_path(&picked)
        )));
        on_export_dir_change(picked);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_folder_is_prepended_and_preset_is_selected_without_duplication() {
        let presets = vec![
            PathBuf::from("/tmp/mock-home/Desktop"),
            PathBuf::from("/tmp/mock-home/Movies"),
        ];

        let (m1, idx1) =
            build_folder_menu_model(Some(Path::new("/tmp/mock-home/Movies")), &presets, 28);
        assert_eq!(idx1, 1);
        assert_eq!(m1.len(), 4); // Desktop, Movies, Separator, ChooseCustom
        assert!(matches!(&m1[1], FolderMenuItem::Dir { selected: true, .. }));

        let custom = Path::new("/tmp/mock-home/Desktop/Captures");
        let (m2, idx2) = build_folder_menu_model(Some(custom), &presets, 28);
        assert_eq!(idx2, 0);
        assert!(matches!(
            &m2[0],
            FolderMenuItem::Dir {
                path,
                selected: true,
                ..
            } if path == custom
        ));
        assert_eq!(m2[1], FolderMenuItem::Separator);
        assert_eq!(m2.last(), Some(&FolderMenuItem::ChooseCustom));
    }

    #[test]
    fn compact_tilde_preserves_leaf_folder_name() {
        if let Some(home) = valid_home_dir() {
            let deep = home.join("Projects/2026/DesignReview/ScreenRecordings");
            let label = compact_tilde_folder(&deep, 26);
            assert_eq!(label, "~/…/ScreenRecordings");
        }
    }
}
