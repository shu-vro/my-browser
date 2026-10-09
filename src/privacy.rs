//! Anti-tracking: WebKit's tracking prevention, third-party cookie blocking,
//! no ad-click attribution, and a built-in tracker blocker compiled from
//! Disconnect's list (resources/trackers.bin, see scripts/update-trackers.py).
//! The browser itself sends nothing anywhere.

use std::cell::RefCell;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{msg_send, sel, MainThreadMarker};
use objc2_foundation::{NSError, NSString};
use objc2_web_kit::{WKContentRuleList, WKContentRuleListStore, WKPreferences, WKWebsiteDataStore};

/// 8-byte little-endian JSON length, then the JSON as raw DEFLATE.
static TRACKERS: &[u8] = include_bytes!("../resources/trackers.bin");

#[link(name = "compression", kind = "dylib")]
unsafe extern "C" {
    fn compression_decode_buffer(
        dst: *mut u8,
        dst_size: usize,
        src: *const u8,
        src_size: usize,
        scratch: *mut std::ffi::c_void,
        algorithm: u32,
    ) -> usize;
}
/// libcompression's COMPRESSION_ZLIB is raw DEFLATE (RFC 1951).
const COMPRESSION_ZLIB: u32 = 0x205;

/// Unpack resources/trackers.bin into the content-blocker JSON.
pub fn tracker_rules_json() -> Option<String> {
    let (len, packed) = TRACKERS.split_at_checked(8)?;
    let len = u64::from_le_bytes(len.try_into().ok()?) as usize;
    let mut out = vec![0u8; len];
    let n = unsafe {
        compression_decode_buffer(out.as_mut_ptr(), len, packed.as_ptr(), packed.len(), std::ptr::null_mut(), COMPRESSION_ZLIB)
    };
    (n == len).then(|| String::from_utf8(out).ok()).flatten()
}

/// Compiled lists are cached on disk by identifier; a new list gets a new one.
fn rules_identifier() -> String {
    // FNV-1a over the packed list.
    let hash = TRACKERS.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("trackers-{hash:016x}")
}

/// Look up the compiled tracker blocker, compiling it on first launch (~1 s).
/// `done` runs once, on the main thread, with the list or None on failure.
pub fn load_tracker_rules(mtm: MainThreadMarker, done: impl FnOnce(Option<Retained<WKContentRuleList>>) + 'static) {
    let Some(store) = (unsafe { WKContentRuleListStore::defaultStore(mtm) }) else {
        return done(None);
    };
    let id = NSString::from_str(&rules_identifier());
    let done = Rc::new(RefCell::new(Some(done)));
    let finish = {
        let done = done.clone();
        move |list: *mut WKContentRuleList| {
            if let Some(f) = done.borrow_mut().take() {
                f(unsafe { Retained::retain(list) });
            }
        }
    };
    let compile = {
        let (store, id, finish) = (store.clone(), id.clone(), finish.clone());
        move || {
            let Some(json) = tracker_rules_json() else { return finish(std::ptr::null_mut()) };
            let finish = finish.clone();
            let block = RcBlock::new(move |list: *mut WKContentRuleList, err: *mut NSError| {
                if let Some(e) = unsafe { err.as_ref() } {
                    eprintln!("tracker blocker failed to compile: {}", e.localizedDescription());
                }
                finish(list)
            });
            unsafe {
                store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
                    Some(&id),
                    Some(&NSString::from_str(&json)),
                    Some(&block),
                )
            };
        }
    };
    let lookup = RcBlock::new(move |list: *mut WKContentRuleList, _err: *mut NSError| {
        if list.is_null() { compile() } else { finish(list) }
    });
    unsafe { store.lookUpContentRuleListForIdentifier_completionHandler(Some(&id), Some(&lookup)) };
}

fn responds(obj: &AnyObject, sel: objc2::runtime::Sel) -> bool {
    unsafe { msg_send![obj, respondsToSelector: sel] }
}

/// Turn on WebKit's tracking prevention and block all third-party cookies
/// (Safari's defaults). Returns (name, applied) for each private setting.
pub fn harden_data_store(store: &WKWebsiteDataStore) -> Vec<(&'static str, bool)> {
    let ok_itp = responds(store, sel!(_setResourceLoadStatisticsEnabled:));
    if ok_itp {
        let _: () = unsafe { msg_send![store, _setResourceLoadStatisticsEnabled: Bool::YES] };
    }
    let ok_cookies = responds(store, sel!(_setThirdPartyCookieBlockingMode:onlyOnSitesWithoutUserInteraction:completionHandler:));
    if ok_cookies {
        let done = RcBlock::new(|| {});
        let _: () = unsafe {
            msg_send![store, _setThirdPartyCookieBlockingMode: Bool::YES, onlyOnSitesWithoutUserInteraction: Bool::NO, completionHandler: &*done]
        };
    }
    vec![("tracking_prevention", ok_itp), ("third_party_cookies_blocked", ok_cookies)]
}

/// No ad-click attribution; safe-browsing lookups only if the user turned them on.
pub fn harden_preferences(prefs: &WKPreferences, safe_browsing: bool) -> Vec<(&'static str, bool)> {
    let ok_pcm = responds(prefs, sel!(_setPrivateClickMeasurementEnabled:));
    if ok_pcm {
        let _: () = unsafe { msg_send![prefs, _setPrivateClickMeasurementEnabled: Bool::NO] };
    }
    unsafe { prefs.setFraudulentWebsiteWarningEnabled(safe_browsing) };
    vec![("ad_attribution_off", ok_pcm)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_list_unpacks_and_has_key_rules() {
        let json = tracker_rules_json().expect("trackers.bin unpacks");
        assert!(json.starts_with('[') && json.ends_with(']'));
        assert!(json.contains(r"doubleclick\\.net"), "ad network blocked");
        assert!(json.contains(r"google-analytics\\.com"), "analytics blocked");
        // Meta's servers are exempt on Meta's own sites, so Messenger keeps working.
        let fb = json.split(r#"{"trigger""#).find(|r| r.contains(r"facebook\\.net[")).expect("facebook.net rule");
        assert!(fb.contains(r#""*messenger.com""#), "{fb}");
        assert!(fb.contains(r#""load-type":["third-party"]"#));
    }

    #[test]
    fn identifier_is_stable() {
        assert_eq!(rules_identifier(), rules_identifier());
        assert!(rules_identifier().starts_with("trackers-"));
    }
}
