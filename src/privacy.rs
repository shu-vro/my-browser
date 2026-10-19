//! Anti-tracking: WebKit's tracking prevention, third-party cookie blocking,
//! no ad-click attribution, and a built-in tracker and ad blocker compiled from
//! Disconnect's list and EasyList's ad servers (resources/trackers.bin, see
//! scripts/update-trackers.py).
//! The browser itself sends nothing anywhere.

use std::cell::RefCell;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{MainThreadMarker, Message, msg_send, sel};
use objc2_foundation::{NSError, NSString};
use objc2_web_kit::{
    WKContentRuleList, WKContentRuleListStore, WKPreferences, WKWebpagePreferences, WKWebsiteDataStore,
};

use crate::store::json_str;

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
        compression_decode_buffer(
            out.as_mut_ptr(),
            len,
            packed.as_ptr(),
            packed.len(),
            std::ptr::null_mut(),
            COMPRESSION_ZLIB,
        )
    };
    (n == len).then(|| String::from_utf8(out).ok()).flatten()
}

/// FNV-1a: names compiled rule lists after their content.
fn fnv(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(seed, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// The tracker list's id. Without exemptions it's the same as before, so the
/// cached compiled list keeps working across upgrades.
fn rules_identifier(exempt: &[String]) -> String {
    let hash = exempt.iter().fold(fnv(TRACKERS, 0xcbf29ce484222325), |h, site| fnv(site.as_bytes(), fnv(b"\0", h)));
    format!("trackers-{hash:016x}")
}

/// A content-blocker regex matching a top-level URL on `host` or any of its
/// subdomains, on any scheme and port.
fn site_regex(host: &str) -> String {
    let mut out = String::from("^[a-z]+://([^/]*\\.)?");
    for c in host.chars() {
        if !c.is_ascii_alphanumeric() && c != '-' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push_str("[:/]");
    out
}

/// Rules that switch off every earlier rule in the same list on these sites.
fn exemptions(sites: &[String]) -> String {
    sites
        .iter()
        .map(|h| {
            format!(
                r#"{{"trigger":{{"url-filter":".*","if-top-url":[{}]}},"action":{{"type":"ignore-previous-rules"}}}}"#,
                json_str(&site_regex(h))
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The tracker blocker plus "Shields down" exemptions for `exempt` sites.
pub fn tracker_rules(exempt: &[String]) -> Option<String> {
    let json = tracker_rules_json()?;
    if exempt.is_empty() {
        return Some(json);
    }
    Some(format!("{},{}]", json.strip_suffix(']')?, exemptions(exempt)))
}

/// Addresses of this Mac and the local network: loopback, private IPv4
/// ranges, link-local (including cloud metadata at 169.254.169.254), IPv6
/// loopback/unique-local/link-local, and mDNS `.local` names. Content-blocker
/// regexes have no alternation, hence one pattern per range.
const LOCAL: [&str; 14] = [
    r"^[a-z]+://localhost[:/]",
    r"^[a-z]+://[^/:]+\.localhost[:/]",
    r"^[a-z]+://127\.[0-9]+\.[0-9]+\.[0-9]+[:/]",
    r"^[a-z]+://0\.0\.0\.0[:/]",
    r"^[a-z]+://10\.[0-9]+\.[0-9]+\.[0-9]+[:/]",
    r"^[a-z]+://192\.168\.[0-9]+\.[0-9]+[:/]",
    r"^[a-z]+://172\.1[6-9]\.[0-9]+\.[0-9]+[:/]",
    r"^[a-z]+://172\.2[0-9]\.[0-9]+\.[0-9]+[:/]",
    r"^[a-z]+://172\.3[01]\.[0-9]+\.[0-9]+[:/]",
    r"^[a-z]+://169\.254\.[0-9]+\.[0-9]+[:/]",
    r"^[a-z]+://\[::1?\][:/]",
    r"^[a-z]+://\[f[cd][0-9a-f:]*\][:/]",
    r"^[a-z]+://\[fe80:[0-9a-z:%.]*\][:/]",
    r"^[a-z]+://[^/:]+\.local[:/]",
];

/// A host on this Mac or the local network (as `store::site_of` writes it):
/// the same ranges as `LOCAL`. Such pages are loaded over plain HTTP as typed,
/// not upgraded to HTTPS first, and the site button says they're local.
pub fn is_local_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6 == "::1" || v6 == "::" || v6.starts_with("fc") || v6.starts_with("fd") || v6.starts_with("fe80:");
    }
    let octets: Vec<u8> = host.split('.').filter_map(|p| p.parse().ok()).collect();
    if octets.len() == 4 && host.split('.').count() == 4 {
        return matches!(octets[..], [127, ..] | [10, ..] | [192, 168, ..] | [169, 254, ..] | [0, 0, 0, 0])
            || matches!(octets[..], [172, b, ..] if (16..=31).contains(&b));
    }
    host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local")
}

/// Websites may not reach the local network (routers, printers, dev servers,
/// apps listening on localhost): such requests are blocked unless the page
/// itself is local, or the user allowed the site. Brave and Chrome guard
/// against the same drive-by attacks.
pub fn local_rules(allowed: &[String]) -> String {
    let top: Vec<String> = LOCAL.iter().map(|p| json_str(p)).collect();
    let mut rules: Vec<String> = top
        .iter()
        .map(|p| {
            format!(
                r#"{{"trigger":{{"url-filter":{p},"unless-top-url":[{}]}},"action":{{"type":"block"}}}}"#,
                top.join(",")
            )
        })
        .collect();
    if !allowed.is_empty() {
        rules.push(exemptions(allowed));
    }
    format!("[{}]", rules.join(","))
}

fn local_identifier(allowed: &[String]) -> String {
    let json = local_rules(allowed);
    format!("local-{:016x}", fnv(json.as_bytes(), 0xcbf29ce484222325))
}

/// Look up a compiled rule list, compiling it if it isn't cached (the tracker
/// list takes ~1 s). Older versions of the same kind are deleted. `done` runs
/// once, on the main thread, with the list or None on failure.
fn load_rules(
    mtm: MainThreadMarker,
    id: String,
    json: impl FnOnce() -> Option<String> + 'static,
    done: impl FnOnce(Option<Retained<WKContentRuleList>>) + 'static,
) {
    let Some(store) = (unsafe { WKContentRuleListStore::defaultStore(mtm) }) else {
        return done(None);
    };
    forget_old_lists(&store, &id);
    let ns_id = NSString::from_str(&id);
    let done = Rc::new(RefCell::new(Some(done)));
    let finish = {
        let done = done.clone();
        move |list: *mut WKContentRuleList| {
            if let Some(f) = done.borrow_mut().take() {
                f(unsafe { Retained::retain(list) });
            }
        }
    };
    let json = RefCell::new(Some(json));
    let compile = {
        let (store, ns_id, finish) = (store.clone(), ns_id.clone(), finish.clone());
        move || {
            let Some(json) = json.take().and_then(|f| f()) else { return finish(std::ptr::null_mut()) };
            let finish = finish.clone();
            let block = RcBlock::new(move |list: *mut WKContentRuleList, err: *mut NSError| {
                if let Some(e) = unsafe { err.as_ref() } {
                    eprintln!("content blocker failed to compile: {}", e.localizedDescription());
                }
                finish(list)
            });
            unsafe {
                store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
                    Some(&ns_id),
                    Some(&NSString::from_str(&json)),
                    Some(&block),
                )
            };
        }
    };
    let lookup = RcBlock::new(
        move |list: *mut WKContentRuleList, _err: *mut NSError| {
            if list.is_null() { compile() } else { finish(list) }
        },
    );
    unsafe { store.lookUpContentRuleListForIdentifier_completionHandler(Some(&ns_id), Some(&lookup)) };
}

/// Compiled lists stay on disk until removed; drop this kind's stale ones.
fn forget_old_lists(store: &WKContentRuleListStore, keep: &str) {
    let prefix = keep.split('-').next().unwrap_or(keep).to_string() + "-";
    let keep = keep.to_string();
    let store2 = store.retain();
    let block = RcBlock::new(move |ids: *mut objc2_foundation::NSArray<NSString>| {
        let Some(ids) = (unsafe { ids.as_ref() }) else { return };
        for id in ids.iter() {
            let s = id.to_string();
            if s.starts_with(&prefix) && s != keep {
                unsafe {
                    store2.removeContentRuleListForIdentifier_completionHandler(
                        Some(&id),
                        Some(&RcBlock::new(|_e: *mut NSError| {})),
                    )
                };
            }
        }
    });
    unsafe { store.getAvailableContentRuleListIdentifiers(Some(&block)) };
}

/// The tracker blocker with "Shields down" sites exempted.
pub fn load_tracker_rules(
    mtm: MainThreadMarker,
    exempt: Vec<String>,
    done: impl FnOnce(Option<Retained<WKContentRuleList>>) + 'static,
) {
    let id = rules_identifier(&exempt);
    load_rules(mtm, id, move || tracker_rules(&exempt), done);
}

/// The local-network shield with the sites the user allowed.
pub fn load_local_rules(
    mtm: MainThreadMarker,
    allowed: Vec<String>,
    done: impl FnOnce(Option<Retained<WKContentRuleList>>) + 'static,
) {
    let id = local_identifier(&allowed);
    load_rules(mtm, id, move || Some(local_rules(&allowed)), done);
}

/// Cookie consent banners of the common consent platforms (OneTrust,
/// Cookiebot, Quantcast, Didomi, Usercentrics, Sourcepoint, TrustArc,
/// Google's Funding Choices, …) and popular plugins, hidden with
/// `css-display-none` rules: no script runs and nothing is clicked, so no
/// consent is ever given. Specific names only, so page content isn't hit.
// ponytail: a banner that also locks scrolling (Sourcepoint, Didomi) leaves
// the page unscrollable; content blockers can only hide. A user script
// restoring `overflow` would fix it, at a script per page.
const BANNERS: [&str; 46] = [
    "#onetrust-banner-sdk",
    "#onetrust-consent-sdk",
    "#CybotCookiebotDialog",
    "#CybotCookiebotDialogBodyUnderlay",
    "#qc-cmp2-container",
    ".qc-cmp2-container",
    "#didomi-host",
    "#usercentrics-root",
    "#usercentrics-cmp-ui",
    "[id^=\"sp_message_container_\"]",
    ".truste_box_overlay",
    ".truste_overlay",
    "#truste-consent-track",
    ".fc-consent-root",
    ".osano-cm-window",
    ".cmplz-cookiebanner",
    ".cky-consent-container",
    ".cky-overlay",
    "#iubenda-cs-banner",
    "#cookie-notice",
    "#cookie-law-info-bar",
    ".cc-window",
    "#gdpr-cookie-message",
    "#cmpbox",
    "#cmpbox2",
    "#axeptio_overlay",
    "#ccc",
    "#hs-eu-cookie-confirmation",
    "#shopify-pc__banner",
    "#BorlabsCookieBox",
    "#termly-code-snippet-support",
    "#cookiescript_injected",
    "#cookiescript_injected_wrapper",
    "#moove_gdpr_cookie_info_bar",
    "#catapult-cookie-bar",
    "#cookie-banner",
    "#cookieBanner",
    "#cookie-consent",
    "#cookieConsent",
    "#cookies-banner",
    ".cookie-banner",
    ".cookie-consent-banner",
    ".js-cookie-consent",
    "#consent-banner",
    ".evidon-banner",
    "#_evidon_banner",
];

/// The cookie-banner hider, shown again on Shields-down sites.
pub fn banner_rules(exempt: &[String]) -> String {
    let mut rules: Vec<String> = BANNERS
        .iter()
        .map(|s| {
            format!(
                r#"{{"trigger":{{"url-filter":".*"}},"action":{{"type":"css-display-none","selector":{}}}}}"#,
                json_str(s)
            )
        })
        .collect();
    if !exempt.is_empty() {
        rules.push(exemptions(exempt));
    }
    format!("[{}]", rules.join(","))
}

pub fn load_banner_rules(
    mtm: MainThreadMarker,
    exempt: Vec<String>,
    done: impl FnOnce(Option<Retained<WKContentRuleList>>) + 'static,
) {
    let json = banner_rules(&exempt);
    let id = format!("banners-{:016x}", fnv(json.as_bytes(), 0xcbf29ce484222325));
    load_rules(mtm, id, move || Some(json), done);
}

/// The user's own filters (Settings → Privacy → My filters, and Hide
/// Element…), in the part of uBlock Origin's and Adblock Plus's syntax a
/// content blocker can do without a script on the page:
/// - `##.ad`, `example.com##.ad`, `~shop.example.com##.ad`: hide elements
///   (everywhere, on those sites and their subdomains, or everywhere but them);
/// - `||ads.example^`, with `$third-party` optionally: block requests to that
///   host and its subdomains;
/// - `! comments` and blank lines.
///
/// Returns the rules (never an empty list: WebKit refuses one) with Shields-down
/// sites exempted, and the lines that aren't understood.
pub fn filter_rules(text: &str, exempt: &[String]) -> (String, Vec<String>) {
    // Matches nothing; keeps the list valid when it has no rules.
    let mut rules = vec![r#"{"trigger":{"url-filter":"^my-browser-none:"},"action":{"type":"block"}}"#.to_string()];
    let mut unknown = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('!')) {
        match filter_rule(line) {
            Some(rule) => rules.push(rule),
            None => unknown.push(line.to_string()),
        }
    }
    if !exempt.is_empty() {
        rules.push(exemptions(exempt));
    }
    (format!("[{}]", rules.join(",")), unknown)
}

fn filter_rule(line: &str) -> Option<String> {
    if let Some((domains, selector)) = line.split_once("##") {
        // `#@#` exceptions and `#?#`/`#$#` extended syntax need scripts.
        if selector.is_empty() || domains.contains('#') || selector.starts_with(['+', '^']) {
            return None;
        }
        let (mut only, mut except) = (Vec::new(), Vec::new());
        for d in domains.split(',').map(|d| d.trim().to_ascii_lowercase()).filter(|d| !d.is_empty()) {
            let (list, name) = match d.strip_prefix('~') {
                Some(name) => (&mut except, name.to_string()),
                None => (&mut only, d),
            };
            if !valid_host(&name) {
                return None;
            }
            list.push(json_str(&format!("*{name}")));
        }
        // WebKit takes sites to include or to leave out, not both.
        let scope = match (only.is_empty(), except.is_empty()) {
            (false, _) => format!(r#","if-domain":[{}]"#, only.join(",")),
            (true, false) => format!(r#","unless-domain":[{}]"#, except.join(",")),
            (true, true) => String::new(),
        };
        return Some(format!(
            r#"{{"trigger":{{"url-filter":".*"{scope}}},"action":{{"type":"css-display-none","selector":{}}}}}"#,
            json_str(selector)
        ));
    }
    let (pattern, options) = line.split_once('$').unwrap_or((line, ""));
    let host = pattern.strip_prefix("||")?.strip_suffix('^')?.to_ascii_lowercase();
    let third_party = match options {
        "" => false,
        "third-party" | "3p" => true,
        _ => return None,
    };
    if !valid_host(&host) {
        return None;
    }
    let load = if third_party { r#","load-type":["third-party"]"# } else { "" };
    Some(format!(
        r#"{{"trigger":{{"url-filter":{}{load}}},"action":{{"type":"block"}}}}"#,
        json_str(&site_regex(&host))
    ))
}

/// A host name as filters write it: letters, digits, dots and dashes.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

pub fn load_filter_rules(
    mtm: MainThreadMarker,
    text: String,
    exempt: Vec<String>,
    done: impl FnOnce(Option<Retained<WKContentRuleList>>) + 'static,
) {
    let (json, _) = filter_rules(&text, &exempt);
    let id = format!("filters-{:016x}", fnv(json.as_bytes(), 0xcbf29ce484222325));
    load_rules(mtm, id, move || Some(json), done);
}

/// Tracking parameters removed from addresses before a page loads. Brave's
/// default list (brave-core query_filter); campaign tags like `utm_*` stay,
/// as in Brave and Firefox, since sites use them for more than tracking.
const TRACKING_PARAMS: [&str; 30] = [
    "__hsfp",
    "__hssc",
    "__hstc",
    "__s",
    "_hsenc",
    "_hsmi",
    "_kx",
    "_openstat",
    "dclid",
    "fbclid",
    "gbraid",
    "gclid",
    "hsCtaTracking",
    "igshid",
    "mc_eid",
    "mkt_tok",
    "ml_subscriber",
    "ml_subscriber_hash",
    "msclkid",
    "oly_anon_id",
    "oly_enc_id",
    "rb_clickid",
    "s_cid",
    "ss_email_id",
    "ttclid",
    "twclid",
    "vero_conv",
    "vero_id",
    "wbraid",
    "yclid",
];

/// `url` without tracking parameters, or None if it has none.
pub fn strip_tracking_params(url: &str) -> Option<String> {
    let (before_fragment, fragment) = match url.find('#') {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    let (base, query) = before_fragment.split_once('?')?;
    let kept: Vec<&str> =
        query.split('&').filter(|kv| !TRACKING_PARAMS.contains(&kv.split('=').next().unwrap_or(""))).collect();
    if kept.len() == query.split('&').count() {
        return None;
    }
    let query = kept.join("&");
    Some(if query.is_empty() { format!("{base}{fragment}") } else { format!("{base}?{query}{fragment}") })
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
    let ok_cookies =
        responds(store, sel!(_setThirdPartyCookieBlockingMode:onlyOnSitesWithoutUserInteraction:completionHandler:));
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

/// Per-page protections, set on every new web view's default page preferences:
/// - HTTPS first: plain-HTTP addresses are tried over HTTPS, falling back to
///   HTTP only if the site has no HTTPS (Brave's "HTTPS by default").
/// - Global Privacy Control: `Sec-GPC: 1` and `navigator.globalPrivacyControl`,
///   a legal opt-out of data sale in some US states (Brave sends it too).
/// - Safari's Advanced Tracking and Fingerprinting Protection: noise in
///   canvas/audio readback and other fingerprinting defenses (Brave's
///   "farbling"), known fingerprinting scripts restricted, and tracking
///   decorations removed from links.
///
/// Returns (name, applied) for each; the private ones may be missing on a
/// future macOS, and the e2e test notices.
pub fn harden_webpage_preferences(prefs: &WKWebpagePreferences, shields: bool) -> Vec<(&'static str, bool)> {
    let ok_https = responds(prefs, sel!(setPreferredHTTPSNavigationPolicy:));
    if ok_https {
        // WKWebpagePreferencesUpgradeToHTTPSPolicyAutomaticFallbackToHTTP
        let _: () = unsafe { msg_send![prefs, setPreferredHTTPSNavigationPolicy: 1isize] };
    }
    let ok_gpc = responds(prefs, sel!(setGlobalPrivacyControlEnabled:));
    if ok_gpc {
        let _: () = unsafe { msg_send![prefs, setGlobalPrivacyControlEnabled: Bool::YES] };
    }
    let ok_fp = responds(prefs, sel!(_setNetworkConnectionIntegrityPolicy:));
    if ok_fp {
        // _WKWebsiteNetworkConnectionIntegrityPolicy: Enabled (baseline
        // protections) | EnhancedTelemetry (fingerprinting protections) |
        // SanitizeLookalikeCharacters (link decoration filtering).
        let policy: usize = if shields { 1 << 0 | 1 << 6 | 1 << 8 } else { 0 };
        let _: () = unsafe { msg_send![prefs, _setNetworkConnectionIntegrityPolicy: policy] };
    }
    vec![("https_first", ok_https), ("global_privacy_control", ok_gpc), ("fingerprinting_protection", ok_fp)]
}

/// A navigation to this Mac or the local network goes where it was typed:
/// no HTTPS-first upgrade, which would only fail or stall there.
pub fn keep_scheme(prefs: &WKWebpagePreferences) {
    if responds(prefs, sel!(setPreferredHTTPSNavigationPolicy:)) {
        // WKWebpagePreferencesUpgradeToHTTPSPolicyKeepAsRequested
        let _: () = unsafe { msg_send![prefs, setPreferredHTTPSNavigationPolicy: 0isize] };
    }
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
        // EasyList's ad servers: the click-hijacking pop-under network on fojik.com.
        assert!(json.contains(r"llvpn\\.com[:/]"), "ad server blocked");
        assert!(json.contains(r#""resource-type":["popup"]"#), "pop-up servers blocked");
    }

    #[test]
    fn identifier_is_stable() {
        assert_eq!(rules_identifier(&[]), rules_identifier(&[]));
        assert!(rules_identifier(&[]).starts_with("trackers-"));
        assert_ne!(rules_identifier(&[]), rules_identifier(&["a.example".into()]));
        assert_ne!(rules_identifier(&["ab".into()]), rules_identifier(&["a".into(), "b".into()]));
    }

    #[test]
    fn exemptions_are_appended_as_valid_json() {
        let json = tracker_rules(&["news.example".into()]).unwrap();
        assert!(json.ends_with(r#"{"trigger":{"url-filter":".*","if-top-url":["^[a-z]+://([^/]*\\.)?news\\.example[:/]"]},"action":{"type":"ignore-previous-rules"}}]"#), "{}", &json[json.len() - 200..]);
        let local = local_rules(&[]);
        assert!(
            local.starts_with('[') && local.ends_with(']') && local.matches(r#""type":"block""#).count() == LOCAL.len()
        );
        assert!(local_rules(&["router.example".into()]).contains("ignore-previous-rules"));
        assert_ne!(local_identifier(&[]), local_identifier(&["x".into()]));
    }

    #[test]
    fn banner_rules_hide_and_exempt() {
        let json = banner_rules(&[]);
        assert!(json.starts_with('[') && json.ends_with(']'));
        assert_eq!(json.matches("css-display-none").count(), BANNERS.len());
        assert!(json.contains(r#""selector":"[id^=\"sp_message_container_\"]""#), "quotes escaped");
        assert!(banner_rules(&["a.example".into()]).ends_with(r#""action":{"type":"ignore-previous-rules"}}]"#));
    }

    #[test]
    fn local_hosts_are_recognised() {
        for local in [
            "localhost",
            "app.localhost",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.20.0.5",
            "169.254.169.254",
            "printer.local",
            "[::1]",
            "[fd00::1]",
            "[fe80::1]",
            "LOCALHOST",
        ] {
            assert!(is_local_host(local), "{local}");
        }
        for public in
            ["example.com", "localhost.example.com", "8.8.8.8", "172.32.0.1", "1.2.3", "[2001:db8::1]", "local"]
        {
            assert!(!is_local_host(public), "{public}");
        }
    }

    #[test]
    fn user_filters_become_rules() {
        let text = "! my filters\n##.ad-box\nnews.example,Blog.Example##div[id^=\"promo\"]\n~shop.example##.x\n\
                    ||ads.example^\n||track.example^$third-party\n\
                    a.example#@#.ad\n/banner/*\n||x.example^$script\nexample.com##+js(nostif)\nb.example##\n\n";
        let (json, unknown) = filter_rules(text, &[]);
        assert!(json.starts_with('[') && json.ends_with(']'));
        assert_eq!(json.matches("css-display-none").count(), 3);
        assert!(json.contains(r#""if-domain":["*news.example","*blog.example"]"#), "{json}");
        assert!(json.contains(r#""selector":"div[id^=\"promo\"]""#));
        assert!(json.contains(r#""unless-domain":["*shop.example"]"#));
        assert!(json.contains(r#""url-filter":"^[a-z]+://([^/]*\\.)?ads\\.example[:/]"},"action":{"type":"block"}"#));
        assert!(json.contains(r#""load-type":["third-party"]"#));
        assert_eq!(
            unknown,
            ["a.example#@#.ad", "/banner/*", "||x.example^$script", "example.com##+js(nostif)", "b.example##"]
        );
        let (empty, _) = filter_rules("", &[]);
        assert_eq!(empty.matches("trigger").count(), 1, "never empty: WebKit refuses an empty list");
        assert!(
            filter_rules("##.ad", &["a.example".into()]).0.ends_with(r#""action":{"type":"ignore-previous-rules"}}]"#)
        );
    }

    #[test]
    fn tracking_parameters_are_stripped() {
        assert_eq!(strip_tracking_params("https://a.example/p?fbclid=1"), Some("https://a.example/p".into()));
        assert_eq!(
            strip_tracking_params("https://a.example/p?q=1&gclid=x&utm_source=y#top"),
            Some("https://a.example/p?q=1&utm_source=y#top".into())
        );
        assert_eq!(strip_tracking_params("https://a.example/p?q=1"), None);
        assert_eq!(strip_tracking_params("https://a.example/p#fbclid=1"), None, "fragments aren't queries");
        assert_eq!(strip_tracking_params("https://a.example/p?xfbclid=1"), None, "exact names only");
    }
}
