//! Profiles: separate browsers for, say, work and home. Each has its own
//! cookies and logins, history, bookmarks, settings and extensions, and runs
//! as its own copy of the app (one running browser per profile, as before).
//!
//! The main profile ("Default") is `~/Library/Application Support/my-browser`;
//! the others are folders in its `profiles/` folder, named as the user named
//! them. File → Profiles and Settings → Profiles open, add and delete them.
//! A window shows its profile in the address bar's placeholder, and the Dock
//! icon carries the name.

use std::path::{Path, PathBuf};

use crate::App;
use crate::store::json_str;

pub const DEFAULT: &str = "Default";

fn main_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Library/Application Support/my-browser")
}

fn root() -> PathBuf {
    main_dir().join("profiles")
}

/// A profile's folder.
pub fn dir_of(name: &str) -> PathBuf {
    if name == DEFAULT { main_dir() } else { root().join(name) }
}

/// The name of the profile in `dir`: Default, a folder in `profiles/`, or
/// None for another folder (`--profile` somewhere else).
pub fn name_of(dir: &Path) -> Option<String> {
    if dir == main_dir() {
        return Some(DEFAULT.into());
    }
    (dir.parent()? == root()).then(|| dir.file_name()?.to_str().map(str::to_string)).flatten()
}

/// Every profile, Default first, then by name.
pub fn all() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root())
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| valid_name(n))
                .collect()
        })
        .unwrap_or_default();
    names.sort_by_key(|n| n.to_lowercase());
    names.insert(0, DEFAULT.into());
    names
}

/// Letters, digits, spaces, `-` and `_`; 1 to 40 characters; not Default.
pub fn valid_name(name: &str) -> bool {
    let n = name.trim();
    n == name
        && (1..=40).contains(&n.chars().count())
        && n.chars().all(|c| c.is_alphanumeric() || " -_".contains(c))
        && !n.eq_ignore_ascii_case(DEFAULT)
}

/// Whether a browser is running with this profile (it holds the lock).
pub fn is_open(dir: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(dir.join("lock")) {
        Ok(f) => f.try_lock().is_err(),
        Err(_) => false,
    }
}

impl App {
    /// This window's profile name: None for the main profile, throwaway
    /// profiles and folders outside `profiles/`.
    pub fn profile_name(&self) -> Option<String> {
        self.profile_dir().and_then(name_of).filter(|n| n != DEFAULT)
    }

    /// Settings → Profiles: `{"current", "profiles": [{"name", "open"}]}`.
    pub fn profiles_json(&self) -> String {
        let current = self.profile_dir().and_then(name_of).unwrap_or_default();
        let items: Vec<String> = all()
            .iter()
            .map(|n| format!("{{\"name\":{},\"open\":{}}}", json_str(n), n == &current || is_open(&dir_of(n))))
            .collect();
        format!("{{\"current\":{},\"profiles\":[{}]}}", json_str(&current), items.join(","))
    }

    pub fn create_profile(&self, name: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err("A profile's name is up to 40 letters, digits, spaces, - or _.".into());
        }
        let dir = dir_of(name);
        if all().iter().any(|n| n.to_lowercase() == name.to_lowercase()) {
            return Err(format!("There's already a profile called {name}."));
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't make the profile: {e}"))
    }

    /// Open a profile in its own copy of the browser (or bring nothing up if
    /// it's this one). Bench mode only reports the folder.
    pub fn open_profile(&self, name: &str) -> Result<String, String> {
        if !all().iter().any(|n| n == name) {
            return Err("no such profile".into());
        }
        let dir = dir_of(name);
        if self.profile_dir() == Some(dir.as_path()) || self.is_bench() {
            return Ok(dir.display().to_string());
        }
        if is_open(&dir) {
            return Err(format!("{name} is already open in another window of my-browser."));
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        std::process::Command::new(exe)
            .arg("--profile")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("Couldn't open the profile: {e}"))?;
        Ok(dir.display().to_string())
    }

    /// Delete a profile and everything in it: its folder, and WebKit's
    /// cookies, site data and extension storage for it. Not Default, not
    /// this one, not one that's open. Asks first (not in bench mode).
    pub fn delete_profile(&self, name: &str, done: crate::internal::Reply) {
        if name == DEFAULT || !all().iter().any(|n| n == name) {
            return done(Err("That profile can't be deleted.".into()));
        }
        let dir = dir_of(name);
        if self.profile_dir() == Some(dir.as_path()) || is_open(&dir) {
            return done(Err("Close that profile's windows first.".into()));
        }
        if !self.is_bench() {
            let alert = objc2_app_kit::NSAlert::new(self.mtm());
            alert.setMessageText(&objc2_foundation::NSString::from_str(&format!("Delete the profile “{name}”?")));
            alert.setInformativeText(&objc2_foundation::NSString::from_str(
                "Its history, bookmarks, logins to websites, settings and extensions are deleted. Passwords and cards \
                 saved in the keychain stay there. This can't be undone.",
            ));
            alert.addButtonWithTitle(&objc2_foundation::NSString::from_str("Delete"));
            alert.addButtonWithTitle(&objc2_foundation::NSString::from_str("Cancel"));
            if alert.runModal() != 1000 {
                return done(Err("cancelled".into()));
            }
        }
        let id = crate::store::Store::open(Some(&dir.join("browser.db"))).ok().and_then(|s| s.get("profile_id"));
        let Some(uuid) = id.as_deref().and_then(|id| {
            objc2_foundation::NSUUID::initWithUUIDString(
                objc2_foundation::NSUUID::alloc(),
                &objc2_foundation::NSString::from_str(id),
            )
        }) else {
            // Never opened: nothing of it in WebKit.
            let _ = std::fs::remove_dir_all(&dir);
            return done(Ok("true".into()));
        };
        let ext_storage =
            crate::internal::webkit_dir().join("WebExtensions").join(id.unwrap_or_default().to_uppercase());
        let done = std::cell::Cell::new(Some(done));
        let block = block2::RcBlock::new(move |_e: *mut objc2_foundation::NSError| {
            let (dir, ext_storage) = (dir.clone(), ext_storage.clone());
            if let Some(done) = done.take() {
                let done = crate::MainOnly(done);
                crate::on_main(move || {
                    let _ = std::fs::remove_dir_all(&ext_storage);
                    let _ = std::fs::remove_dir_all(&dir);
                    (done.0)(Ok("true".into()));
                });
            }
        });
        unsafe {
            objc2_web_kit::WKWebsiteDataStore::removeDataStoreForIdentifier_completionHandler(&uuid, &block, self.mtm())
        };
    }
}

use objc2::{AnyThread, MainThreadOnly};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names() {
        for ok in ["Work", "home 2", "Ünï_côde-x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", " Work", "a/b", "..", "default", "Default", &"x".repeat(41), "tab\there"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(dir_of("Work"), root().join("Work"));
        assert_eq!(name_of(&root().join("Work")).as_deref(), Some("Work"));
        assert_eq!(name_of(&main_dir()).as_deref(), Some(DEFAULT));
        assert_eq!(name_of(Path::new("/tmp/elsewhere")), None);
    }
}
