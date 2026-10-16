//! Changing keyboard shortcuts from Settings → Shortcuts. Every shortcut
//! is still a menu item's key equivalent; a changed one is kept in the
//! profile (`shortcut:<id>` settings rows) and put on the menu item at
//! launch and when changed. macOS's own App Shortcuts keep working on top.

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenuItem};
use objc2_foundation::NSString;

use crate::App;
use crate::store::json_str;

/// A menu item's stable name: its action and tag (`newTab:#0`,
/// `selectTabByNumber:#3`), the same whatever its title says now, and `.2`
/// for a second item with the same action (Next Tab beside Show Next Tab).
fn base_id(item: &NSMenuItem) -> Option<String> {
    let action = item.action()?;
    Some(format!("{}#{}", action.name().to_str().ok()?, item.tag()))
}

/// The menu bar's items that take a shortcut: (menu title, id, item). Not
/// the window list, bookmarks, separators or what macOS adds (Dictation, Emoji).
fn items(app: &App) -> Vec<(String, String, Retained<NSMenuItem>)> {
    let Some(bar) = NSApplication::sharedApplication(app.mtm()).mainMenu() else { return Vec::new() };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for holder in bar.itemArray() {
        let Some(menu) = holder.submenu() else { continue };
        for item in menu.itemArray() {
            let Some(base) = base_id(&item) else { continue };
            let system =
                ["makeKeyAndOrderFront:", "openBookmark:", "startDictation:", "orderFrontCharacterPalette:", "_"];
            if item.isSeparatorItem() || item.hasSubmenu() || system.iter().any(|p| base.starts_with(p)) {
                continue;
            }
            let n = seen.iter().filter(|b| **b == base).count();
            seen.push(base.clone());
            let id = if n == 0 { base } else { format!("{base}.{}", n + 1) };
            out.push((menu.title().to_string(), id, item));
        }
    }
    out
}

fn find(app: &App, id: &str) -> Option<Retained<NSMenuItem>> {
    items(app).into_iter().find(|(_, i, _)| i == id).map(|(.., item)| item)
}

/// `cmd+shift+k`-style keys to a key equivalent: (key, modifiers). Shift
/// with a letter or symbol is written AppKit's way, as the shifted
/// character (`K`, `{`) without the Shift flag.
pub fn parse(spec: &str) -> Result<(String, NSEventModifierFlags), String> {
    let mut rest = spec;
    let mut mods = NSEventModifierFlags::empty();
    loop {
        let found = [
            ("cmd+", NSEventModifierFlags::Command),
            ("alt+", NSEventModifierFlags::Option),
            ("ctrl+", NSEventModifierFlags::Control),
            ("shift+", NSEventModifierFlags::Shift),
        ]
        .into_iter()
        .find_map(|(p, f)| rest.strip_prefix(p).map(|r| (r, f)));
        let Some((r, f)) = found else { break };
        mods |= f;
        rest = r;
    }
    let key = match rest {
        "tab" => "\t".to_string(),
        "left" => "\u{F702}".to_string(),
        "right" => "\u{F703}".to_string(),
        "up" => "\u{F700}".to_string(),
        "down" => "\u{F701}".to_string(),
        k if k.chars().count() == 1 && !k.chars().any(char::is_whitespace) => k.to_string(),
        _ => return Err(format!("can't use {spec:?}: one key, after its modifiers")),
    };
    if !mods.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Control | NSEventModifierFlags::Option) {
        return Err("a shortcut needs ⌘, ⌥ or ⌃, or it would stop you typing that key".into());
    }
    let c = key.chars().next().unwrap();
    if mods.contains(NSEventModifierFlags::Shift) && (c.is_ascii_alphabetic() || c.is_ascii_punctuation()) {
        mods.remove(NSEventModifierFlags::Shift);
        return Ok((key.to_uppercase(), mods));
    }
    let key = if c.is_ascii_uppercase() { key } else { key.to_lowercase() };
    Ok((key, mods))
}

/// How a key equivalent reads: `⌃⌥⇧⌘K`. Uppercase letters and shifted
/// symbols imply Shift.
pub fn display(key: &str, m: NSEventModifierFlags) -> String {
    if key.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (flag, sign) in [
        (NSEventModifierFlags::Control, "⌃"),
        (NSEventModifierFlags::Option, "⌥"),
        (NSEventModifierFlags::Shift, "⇧"),
        (NSEventModifierFlags::Command, "⌘"),
    ] {
        let shift = flag == NSEventModifierFlags::Shift && key.chars().any(char::is_uppercase);
        if m.contains(flag) || shift {
            out.push_str(sign);
        }
    }
    out.push_str(&match key {
        "\t" => "⇥".to_string(),
        "\u{F702}" => "←".to_string(),
        "\u{F703}" => "→".to_string(),
        "\u{F700}" => "↑".to_string(),
        "\u{F701}" => "↓".to_string(),
        k => k.to_uppercase(),
    });
    out
}

/// Remember every item's built-in shortcut, then put the profile's changes
/// on the menu. At launch, once the profile is open.
pub fn apply_saved(app: &App) {
    for (_, id, item) in items(app) {
        app.default_keys()
            .borrow_mut()
            .insert(id.clone(), (item.keyEquivalent().to_string(), item.keyEquivalentModifierMask()));
        if let Some(spec) = app.store().get(&format!("shortcut:{id}")) {
            set_item(&item, &spec);
        }
    }
}

fn set_item(item: &NSMenuItem, spec: &str) {
    let (key, mods) = if spec.is_empty() {
        (String::new(), NSEventModifierFlags::empty())
    } else {
        match parse(spec) {
            Ok(k) => k,
            Err(_) => return,
        }
    };
    item.setKeyEquivalent(&NSString::from_str(&key));
    item.setKeyEquivalentModifierMask(mods);
}

/// Give `id` the keys `spec` ("" = no shortcut). Refused if another item
/// already uses them.
pub fn set(app: &App, id: &str, spec: &str) -> Result<(), String> {
    let item = find(app, id).ok_or("no such menu item")?;
    if !spec.is_empty() {
        let (key, mods) = parse(spec)?;
        let taken = items(app).into_iter().find(|(_, other_id, other)| {
            other_id != id && other.keyEquivalent().to_string() == key && other.keyEquivalentModifierMask() == mods
        });
        if let Some((menu, _, other)) = taken {
            return Err(format!("{} is already {menu} → {}", display(&key, mods), other.title()));
        }
    }
    app.store().put(&format!("shortcut:{id}"), spec);
    set_item(&item, spec);
    Ok(())
}

/// Back to the built-in shortcut.
pub fn reset(app: &App, id: &str) -> Result<(), String> {
    let item = find(app, id).ok_or("no such menu item")?;
    app.store().delete(&format!("shortcut:{id}"));
    if let Some((key, mods)) = app.default_keys().borrow().get(id).cloned() {
        item.setKeyEquivalent(&NSString::from_str(&key));
        item.setKeyEquivalentModifierMask(mods);
    }
    Ok(())
}

/// Every item, read from the menu bar itself (so it also shows changes
/// made in System Settings → Keyboard → App Shortcuts):
/// `[{"id","menu","title","keys","custom"}]`.
pub fn json(app: &App) -> String {
    let rows: Vec<String> = items(app)
        .iter()
        .map(|(menu, id, item)| {
            let keys = display(&item.keyEquivalent().to_string(), item.keyEquivalentModifierMask());
            let custom = app.store().get(&format!("shortcut:{id}")).is_some();
            format!(
                "{{\"id\":{},\"menu\":{},\"title\":{},\"keys\":{},\"custom\":{custom}}}",
                json_str(id),
                json_str(menu),
                json_str(&item.title().to_string()),
                json_str(&keys)
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_the_way_appkit_writes_them() {
        let (cmd, alt, shift, ctrl) = (
            NSEventModifierFlags::Command,
            NSEventModifierFlags::Option,
            NSEventModifierFlags::Shift,
            NSEventModifierFlags::Control,
        );
        assert_eq!(parse("cmd+k").unwrap(), ("k".into(), cmd));
        assert_eq!(parse("cmd+shift+k").unwrap(), ("K".into(), cmd), "Shift is the capital");
        assert_eq!(parse("cmd+shift+{").unwrap(), ("{".into(), cmd));
        assert_eq!(parse("cmd++").unwrap(), ("+".into(), cmd));
        assert_eq!(parse("ctrl+shift+tab").unwrap(), ("\t".into(), ctrl | shift), "Shift stays with Tab");
        assert_eq!(parse("cmd+alt+left").unwrap(), ("\u{F702}".into(), cmd | alt));
        assert!(parse("k").is_err() && parse("shift+k").is_err(), "plain keys would eat typing");
        assert!(parse("cmd+kk").is_err() && parse("cmd+").is_err());
        assert_eq!(display("K", cmd), "⇧⌘K");
        assert_eq!(display("\t", ctrl | shift), "⌃⇧⇥");
        assert_eq!(display("k", cmd | alt), "⌥⌘K");
    }
}
