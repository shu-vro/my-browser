//! Saved passwords, in the macOS Keychain.
//!
//! Apple's Passwords app only lets Chrome, Edge and Firefox in (its helper
//! refuses other parents at launch), so the browser keeps its logins in the
//! user's login keychain as internet passwords, marked with our creator code.
//! They show in Keychain Access, are encrypted by macOS, and reading one is
//! guarded by the keychain's own access control. Throwaway profiles (tests,
//! benchmarks) keep them in memory instead and never touch the keychain.
//!
//! resources/passwords.js (in its own script world) reports sign-ins and
//! login-field focus; filling only happens when the user picks an account in
//! a native menu, and only on the site the password was saved for.

use std::cell::RefCell;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSMutableDictionary, NSNumber, NSString};

/// One saved login: (site, username).
pub type Login = (String, String);

/// Where logins live: the keychain, or memory for throwaway profiles.
pub enum Vault {
    Keychain,
    Memory(RefCell<Vec<(String, String, String)>>),
}

/// Sites passwords are saved and filled on: HTTPS, or plain HTTP on this
/// Mac (local development). Elsewhere a password would travel unencrypted.
pub fn site_for(url: &str) -> Option<String> {
    let site = crate::store::site_of(url)?;
    let local = matches!(site.as_str(), "localhost" | "127.0.0.1" | "[::1]");
    (url.starts_with("https://") || (url.starts_with("http://") && local)).then_some(site)
}

impl Vault {
    /// Usernames saved for `site`, sorted.
    pub fn accounts(&self, site: &str) -> Vec<String> {
        let mut out: Vec<String> = self.all().into_iter().filter(|(s, _)| s == site).map(|(_, u)| u).collect();
        out.sort();
        out
    }

    /// Every saved (site, username), by site.
    pub fn all(&self) -> Vec<Login> {
        let mut out = match self {
            Vault::Memory(m) => m.borrow().iter().map(|(s, u, _)| (s.clone(), u.clone())).collect(),
            Vault::Keychain => keychain::all(),
        };
        out.sort();
        out
    }

    pub fn password(&self, site: &str, user: &str) -> Option<String> {
        match self {
            Vault::Memory(m) => m.borrow().iter().find(|(s, u, _)| s == site && u == user).map(|(.., p)| p.clone()),
            Vault::Keychain => keychain::password(site, user),
        }
    }

    pub fn save(&self, site: &str, user: &str, password: &str) -> Result<(), String> {
        match self {
            Vault::Memory(m) => {
                let mut m = m.borrow_mut();
                m.retain(|(s, u, _)| !(s == site && u == user));
                m.push((site.into(), user.into(), password.into()));
                Ok(())
            }
            Vault::Keychain => keychain::save(site, user, password),
        }
    }

    pub fn delete(&self, site: &str, user: &str) {
        match self {
            Vault::Memory(m) => m.borrow_mut().retain(|(s, u, _)| !(s == site && u == user)),
            Vault::Keychain => keychain::delete(site, user),
        }
    }
}

/// The Security framework's item API. Its dictionaries are CFDictionaries,
/// which are NSDictionaries (toll-free bridged); the keys are CFStrings.
mod keychain {
    use super::*;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: &'static NSString;
        static kSecClassInternetPassword: &'static NSString;
        static kSecAttrServer: &'static NSString;
        static kSecAttrAccount: &'static NSString;
        static kSecAttrProtocol: &'static NSString;
        static kSecAttrProtocolHTTPS: &'static NSString;
        static kSecAttrCreator: &'static NSString;
        static kSecAttrLabel: &'static NSString;
        static kSecValueData: &'static NSString;
        static kSecReturnAttributes: &'static NSString;
        static kSecReturnData: &'static NSString;
        static kSecMatchLimit: &'static NSString;
        static kSecMatchLimitAll: &'static NSString;
        static kSecMatchLimitOne: &'static NSString;
        fn SecItemAdd(attributes: *const AnyObject, result: *mut *const c_void) -> i32;
        fn SecItemCopyMatching(query: *const AnyObject, result: *mut *const c_void) -> i32;
        fn SecItemUpdate(query: *const AnyObject, attributes: *const AnyObject) -> i32;
        fn SecItemDelete(query: *const AnyObject) -> i32;
    }

    /// Our creator code, 'mybr': marks the items this browser saved.
    const CREATOR: u32 = u32::from_be_bytes(*b"mybr");
    const ERR_DUPLICATE: i32 = -25299;

    fn dict(pairs: &[(&NSString, &AnyObject)]) -> Retained<NSMutableDictionary<AnyObject, AnyObject>> {
        let d = NSMutableDictionary::<AnyObject, AnyObject>::new();
        for (k, v) in pairs {
            let _: () = unsafe { objc2::msg_send![&*d, setObject: *v, forKey: *k] };
        }
        d
    }

    /// The query for our items, narrowed to a site and user if given.
    fn query(site: Option<&str>, user: Option<&str>) -> Retained<NSMutableDictionary<AnyObject, AnyObject>> {
        let creator = NSNumber::new_u32(CREATOR);
        let q = unsafe { dict(&[(kSecClass, kSecClassInternetPassword), (kSecAttrCreator, &creator)]) };
        let set = |k: &NSString, v: &str| {
            let v = NSString::from_str(v);
            let _: () = unsafe { objc2::msg_send![&*q, setObject: &*v, forKey: k] };
        };
        if let Some(s) = site {
            set(unsafe { kSecAttrServer }, s);
        }
        if let Some(u) = user {
            set(unsafe { kSecAttrAccount }, u);
        }
        q
    }

    fn copy_matching(q: &NSMutableDictionary<AnyObject, AnyObject>) -> Option<Retained<AnyObject>> {
        let mut out: *const c_void = std::ptr::null();
        let status = unsafe { SecItemCopyMatching(q as *const _ as *const AnyObject, &mut out) };
        // Copy rule: we own the result.
        (status == 0).then(|| unsafe { Retained::from_raw(out as *mut AnyObject) }).flatten()
    }

    pub fn all() -> Vec<Login> {
        let q = query(None, None);
        let _: () =
            unsafe { objc2::msg_send![&*q, setObject: &*NSNumber::new_bool(true), forKey: kSecReturnAttributes] };
        let _: () = unsafe { objc2::msg_send![&*q, setObject: kSecMatchLimitAll, forKey: kSecMatchLimit] };
        let Some(items) = copy_matching(&q).and_then(|r| r.downcast::<NSArray>().ok()) else { return Vec::new() };
        let text = |d: &NSDictionary, k: &NSString| {
            d.objectForKey(k).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string())
        };
        items
            .iter()
            .filter_map(|i| {
                let d = i.downcast_ref::<NSDictionary>()?;
                Some((text(d, unsafe { kSecAttrServer })?, text(d, unsafe { kSecAttrAccount }).unwrap_or_default()))
            })
            .collect()
    }

    pub fn password(site: &str, user: &str) -> Option<String> {
        let q = query(Some(site), Some(user));
        let _: () = unsafe { objc2::msg_send![&*q, setObject: &*NSNumber::new_bool(true), forKey: kSecReturnData] };
        let _: () = unsafe { objc2::msg_send![&*q, setObject: kSecMatchLimitOne, forKey: kSecMatchLimit] };
        let data = copy_matching(&q)?.downcast::<NSData>().ok()?;
        String::from_utf8(data.to_vec()).ok()
    }

    pub fn save(site: &str, user: &str, password: &str) -> Result<(), String> {
        let data = NSData::with_bytes(password.as_bytes());
        let q = query(Some(site), Some(user));
        let label = NSString::from_str(site);
        let add = query(Some(site), Some(user));
        unsafe {
            let _: () = objc2::msg_send![&*add, setObject: &*data, forKey: kSecValueData];
            let _: () = objc2::msg_send![&*add, setObject: kSecAttrProtocolHTTPS, forKey: kSecAttrProtocol];
            let _: () = objc2::msg_send![&*add, setObject: &*label, forKey: kSecAttrLabel];
        }
        let mut status = unsafe { SecItemAdd(&*add as *const _ as *const AnyObject, std::ptr::null_mut()) };
        if status == ERR_DUPLICATE {
            let change = unsafe { dict(&[(kSecValueData, &data)]) };
            status =
                unsafe { SecItemUpdate(&*q as *const _ as *const AnyObject, &*change as *const _ as *const AnyObject) };
        }
        if status == 0 { Ok(()) } else { Err(format!("The keychain refused to save it (error {status}).")) }
    }

    pub fn delete(site: &str, user: &str) {
        let q = query(Some(site), Some(user));
        unsafe { SecItemDelete(&*q as *const _ as *const AnyObject) };
    }
}

/// Parse a passwords CSV as exported by Apple's Passwords app, Safari,
/// Chrome, Edge, Brave or Firefox: a header row naming the url, username and
/// password columns (in any order), then one login per row. Returns
/// (url, username, password) for each row that has a url and a password.
pub fn parse_csv(text: &str) -> Vec<(String, String, String)> {
    let rows = csv_rows(text);
    let Some((header, rows)) = rows.split_first() else { return Vec::new() };
    let col = |names: &[&str]| header.iter().position(|h| names.contains(&h.trim().to_ascii_lowercase().as_str()));
    let (Some(url), Some(pass)) = (col(&["url", "website", "login_uri", "origin"]), col(&["password"])) else {
        return Vec::new();
    };
    let user = col(&["username", "login", "user", "login_username", "email"]);
    rows.iter()
        .filter_map(|r| {
            let get = |i: usize| r.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
            let (u, p) = (get(url), get(pass));
            (!u.is_empty() && !p.is_empty()).then(|| (u, user.map(get).unwrap_or_default(), p))
        })
        .collect()
}

/// RFC 4180 CSV: quoted fields may hold commas, quotes ("") and newlines.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let (mut row, mut field, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => row.push(std::mem::take(&mut field)),
            ('\r', false) => {}
            ('\n', false) => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (c, _) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|r| r.iter().any(|f| !f.is_empty()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_kept_only_for_secure_or_local_sites() {
        assert_eq!(site_for("https://Login.Example.com/x").as_deref(), Some("login.example.com"));
        assert_eq!(site_for("http://127.0.0.1:8080/login").as_deref(), Some("127.0.0.1"));
        assert_eq!(site_for("http://example.com/login"), None, "would travel unencrypted");
        assert_eq!(site_for("mybrowser://settings"), None);
    }

    #[test]
    fn the_memory_vault_saves_updates_and_deletes() {
        let v = Vault::Memory(RefCell::new(Vec::new()));
        v.save("a.example", "bob", "1").unwrap();
        v.save("a.example", "alice", "2").unwrap();
        v.save("a.example", "bob", "3").unwrap();
        v.save("b.example", "carol", "4").unwrap();
        assert_eq!(v.accounts("a.example"), ["alice", "bob"]);
        assert_eq!(v.password("a.example", "bob").as_deref(), Some("3"), "updated, not duplicated");
        assert_eq!(v.all().len(), 3);
        v.delete("a.example", "bob");
        assert_eq!(v.accounts("a.example"), ["alice"]);
        assert_eq!(v.password("b.example", "alice"), None, "per site");
    }

    #[test]
    fn csv_exports_of_every_browser_parse() {
        // Apple Passwords / Safari
        let apple = "Title,URL,Username,Password,Notes,OTPAuth\nExample,https://a.example/,alice,\"p,w\"\"1\",,\n";
        assert_eq!(parse_csv(apple), [("https://a.example/".into(), "alice".into(), "p,w\"1".into())]);
        // Chrome / Edge / Brave (with a byte-order mark and CRLF)
        let chrome = "\u{feff}name,url,username,password,note\r\nb,https://b.example/login,bob,secret,\r\n";
        assert_eq!(parse_csv(chrome), [("https://b.example/login".into(), "bob".into(), "secret".into())]);
        // Firefox
        let firefox =
            "\"url\",\"username\",\"password\",\"httpRealm\"\n\"https://c.example\",\"carol\",\"multi\nline\",\"\"\n";
        assert_eq!(parse_csv(firefox), [("https://c.example".into(), "carol".into(), "multi\nline".into())]);
        assert!(parse_csv("just,some,columns\n1,2,3\n").is_empty(), "no url/password columns");
        assert!(parse_csv("").is_empty());
    }

    /// Writes to and deletes from the real login keychain, so it's opt-in:
    /// `cargo test --release -- --ignored keychain`.
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let site = format!("my-browser-test-{}.invalid", std::process::id());
        keychain::save(&site, "tester", "first").unwrap();
        keychain::save(&site, "tester", "second").unwrap();
        assert_eq!(keychain::password(&site, "tester").as_deref(), Some("second"));
        assert!(keychain::all().contains(&(site.clone(), "tester".into())));
        keychain::delete(&site, "tester");
        assert_eq!(keychain::password(&site, "tester"), None);
    }
}
