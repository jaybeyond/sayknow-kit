// System-wide shortcuts. The frontend lists the same set in
// src/lib/shortcuts.ts; the test at the bottom keeps the two in step.
//
// ⌃⌥ on macOS: ⌘⇧ letters are nearly all browser shortcuts (⌘⇧T reopens a
// closed tab, which is what the old ⌘⇧T open key collided with), and ⌃⌥
// types no characters. Elsewhere Ctrl+Alt is AltGr, so Ctrl+Alt+2 is how a
// Spanish keyboard types "@"; adding Shift keeps every character reachable.

use std::sync::Mutex;

use serde::Serialize;
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};

/// (id, key). Ids match the frontend registry.
pub const GLOBAL_SHORTCUTS: &[(&str, Code)] = &[
    ("global.toggle", Code::KeyS),
    ("global.translate", Code::Digit1),
    ("global.chat", Code::Digit2),
    ("global.clipboard", Code::Digit3),
    ("global.tools", Code::Digit4),
    ("global.newMemo", Code::KeyM),
];

pub fn modifiers() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::CONTROL | Modifiers::ALT
    } else {
        Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT
    }
}

pub fn bindings() -> Vec<(&'static str, Shortcut)> {
    GLOBAL_SHORTCUTS
        .iter()
        .map(|&(id, code)| (id, Shortcut::new(Some(modifiers()), code)))
        .collect()
}

/// What the popover should do for a shortcut id: `None` for the plain
/// show/hide toggle, otherwise the navigation target sent to the webview
/// ("translate", "chat", "clipboard", "tools", "newMemo").
pub fn target(id: &str) -> Option<&str> {
    match id {
        "global.toggle" => None,
        other => other.strip_prefix("global."),
    }
}

#[derive(Clone, Serialize)]
pub struct ShortcutStatus {
    pub id: String,
    /// False when the OS refused the key, usually because another app owns it.
    pub registered: bool,
}

#[derive(Default)]
pub struct Status(pub Mutex<Vec<ShortcutStatus>>);

#[tauri::command]
pub fn get_global_shortcuts(status: tauri::State<'_, Status>) -> Vec<ShortcutStatus> {
    status.0.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_name(code: Code) -> String {
        let name = format!("{code:?}");
        name.strip_prefix("Key")
            .or_else(|| name.strip_prefix("Digit"))
            .unwrap_or(&name)
            .to_string()
    }

    #[test]
    fn frontend_registry_lists_exactly_the_registered_global_shortcuts() {
        let ts = include_str!("../../src/lib/shortcuts.ts");
        for &(id, code) in GLOBAL_SHORTCUTS {
            let entry = format!(
                r#"{{ id: "{id}", group: "global", combo: "Global+{}","#,
                key_name(code)
            );
            assert!(ts.contains(&entry), "shortcuts.ts is missing {entry}");
        }
        assert_eq!(
            ts.matches(r#"group: "global""#).count(),
            GLOBAL_SHORTCUTS.len(),
            "shortcuts.ts lists a global shortcut Rust does not register"
        );
    }

    #[test]
    fn global_shortcuts_are_unique_and_leave_the_browser_reopen_tab_key_alone() {
        let all = bindings();
        for (i, (_, a)) in all.iter().enumerate() {
            assert!(all[i + 1..].iter().all(|(_, b)| b != a), "duplicate {a:?}");
        }
        for mods in [Modifiers::SUPER | Modifiers::SHIFT, Modifiers::CONTROL | Modifiers::SHIFT] {
            let reopen_tab = Shortcut::new(Some(mods), Code::KeyT);
            assert!(all.iter().all(|(_, s)| *s != reopen_tab));
        }
    }

    #[test]
    fn non_mac_global_shortcuts_never_equal_altgr_alone() {
        // AltGr arrives as Ctrl+Alt; a hotkey on exactly Ctrl+Alt would eat
        // characters such as "@" on a Spanish layout.
        if cfg!(target_os = "macos") {
            assert_eq!(modifiers(), Modifiers::CONTROL | Modifiers::ALT);
        } else {
            assert!(modifiers().contains(Modifiers::SHIFT));
        }
    }

    #[test]
    fn toggle_has_no_target_and_the_rest_name_their_panel() {
        assert_eq!(target("global.toggle"), None);
        assert_eq!(target("global.clipboard"), Some("clipboard"));
        assert_eq!(target("global.newMemo"), Some("newMemo"));
    }
}
