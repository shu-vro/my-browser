//! Site data: what websites keep on this Mac (cookies, local storage,
//! databases, caches…), by site, as WebKit's data store records it. The site
//! menu's "Site Data…" sheet shows the current site's, Settings → Storage →
//! Site data shows every site's; both delete one site's data.

use std::cell::RefCell;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadOnly, Message, sel};
use objc2_app_kit::{NSAlert, NSButton, NSStackView, NSTextField, NSView};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{WKWebsiteDataRecord, WKWebsiteDataStore};

use crate::store::json_str;
use crate::tab::Tab;

/// One site's data: its name (usually the registrable domain) and kinds.
pub struct Record {
    pub name: String,
    pub kinds: Vec<&'static str>,
    record: Retained<WKWebsiteDataRecord>,
}

/// A WebKit data type as a person would call it.
fn kind(t: &str) -> &'static str {
    match t.strip_prefix("WKWebsiteDataType").unwrap_or(t) {
        "Cookies" => "Cookies",
        "LocalStorage" => "Local storage",
        "SessionStorage" => "Session storage",
        "IndexedDBDatabases" | "WebSQLDatabases" => "Databases",
        "DiskCache" | "MemoryCache" | "FetchCache" | "OfflineWebApplicationCache" => "Cache",
        "ServiceWorkerRegistrations" => "Offline workers",
        "FileSystem" => "Files",
        "MediaKeys" => "Media licenses",
        _ => "Other",
    }
}

/// Every site's data, by name. `done` runs on the main thread.
pub fn fetch(store: &WKWebsiteDataStore, done: impl FnOnce(Vec<Record>) + 'static) {
    let types = unsafe { WKWebsiteDataStore::allWebsiteDataTypes(store.mtm()) };
    let done = RefCell::new(Some(done));
    let block = RcBlock::new(move |records: std::ptr::NonNull<NSArray<WKWebsiteDataRecord>>| {
        let records = unsafe { records.as_ref() };
        let mut out: Vec<Record> = records
            .iter()
            .map(|r| {
                let mut kinds: Vec<&str> = unsafe { r.dataTypes() }.iter().map(|t| kind(&t.to_string())).collect();
                kinds.sort();
                kinds.dedup();
                Record { name: unsafe { r.displayName() }.to_string(), kinds, record: r }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(f) = done.take() {
            f(out);
        }
    });
    unsafe { store.fetchDataRecordsOfTypes_completionHandler(&types, &block) };
}

/// Delete everything the site called `name` keeps. `done(found)`.
pub fn delete(store: &WKWebsiteDataStore, name: &str, done: impl FnOnce(bool) + 'static) {
    let (store2, name) = (store.retain(), name.to_string());
    fetch(store, move |records| {
        let matching: Vec<Retained<WKWebsiteDataRecord>> =
            records.into_iter().filter(|r| r.name == name).map(|r| r.record).collect();
        if matching.is_empty() {
            return done(false);
        }
        let types = unsafe { WKWebsiteDataStore::allWebsiteDataTypes(store2.mtm()) };
        let done = RefCell::new(Some(done));
        let block = RcBlock::new(move || {
            if let Some(f) = done.take() {
                f(true);
            }
        });
        unsafe {
            store2.removeDataOfTypes_forDataRecords_completionHandler(
                &types,
                &NSArray::from_retained_slice(&matching),
                &block,
            )
        };
    });
}

/// Whether a record named `name` belongs to the page's `host`: the same
/// site, a parent domain of it (github.com for gist.github.com) or one of
/// its subdomains.
pub fn belongs(name: &str, host: &str) -> bool {
    let (name, host) = (name.to_ascii_lowercase(), host.to_ascii_lowercase());
    name == host || host.ends_with(&format!(".{name}")) || name.ends_with(&format!(".{host}"))
}

pub fn json(records: &[Record]) -> String {
    let items: Vec<String> = records
        .iter()
        .map(|r| {
            let kinds: Vec<String> = r.kinds.iter().map(|k| json_str(k)).collect();
            format!("{{\"name\":{},\"kinds\":[{}]}}", json_str(&r.name), kinds.join(","))
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// The site menu's "Site Data…": the data of the site you're on, each with
/// a delete button, as a sheet. "Manage All Site Data…" opens Settings.
pub fn show(tab: &Tab, store: &WKWebsiteDataStore, host: &str) {
    let (tab, host) = (tab.retain(), host.to_string());
    fetch(store, move |records| {
        let mtm = tab.mtm();
        let mine: Vec<&Record> = records.iter().filter(|r| belongs(&r.name, &host)).collect();
        if tab.app_is_bench() {
            let rows: Vec<String> = mine.iter().map(|r| format!("{}:{}", r.name, r.kinds.join(","))).collect();
            say!("SITEDATA {}", rows.join("|"));
        }
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("On-device site data"));
        alert.setInformativeText(&NSString::from_str(if mine.is_empty() {
            "This site keeps nothing on your Mac."
        } else {
            "What this site and its subdomains keep on your Mac: sign-ins (cookies), saved settings and offline \
             copies. Deleting signs you out of the site."
        }));
        alert.addButtonWithTitle(&NSString::from_str("Done"));
        alert.addButtonWithTitle(&NSString::from_str("Manage All Site Data…"));
        let w = 380.0;
        let rows: Vec<Retained<NSView>> = mine
            .iter()
            .map(|r| {
                let row = NSView::initWithFrame(
                    NSView::alloc(mtm),
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, 36.0)),
                );
                let name = NSTextField::labelWithString(&NSString::from_str(&r.name), mtm);
                name.setFrame(NSRect::new(NSPoint::new(0.0, 18.0), NSSize::new(w - 40.0, 17.0)));
                let kinds = NSTextField::labelWithString(&NSString::from_str(&r.kinds.join(", ")), mtm);
                kinds.setFrame(NSRect::new(NSPoint::new(0.0, 1.0), NSSize::new(w - 40.0, 16.0)));
                kinds.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
                kinds.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
                let trash = unsafe {
                    NSButton::buttonWithImage_target_action(
                        &objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(
                            &NSString::from_str("trash"),
                            Some(&NSString::from_str(&format!("Delete data of {}", r.name))),
                        )
                        .unwrap(),
                        Some(&*tab),
                        Some(sel!(deleteSiteData:)),
                        mtm,
                    )
                };
                trash.setBordered(false);
                trash.setFrame(NSRect::new(NSPoint::new(w - 30.0, 6.0), NSSize::new(28.0, 24.0)));
                trash.setToolTip(Some(&NSString::from_str(&format!("Delete everything {} keeps", r.name))));
                // The record's name rides on the button, for `deleteSiteData:`.
                objc2_app_kit::NSUserInterfaceItemIdentification::setIdentifier(
                    &*trash,
                    Some(&NSString::from_str(&r.name)),
                );
                row.addSubview(&name);
                row.addSubview(&kinds);
                row.addSubview(&trash);
                row
            })
            .collect();
        if !rows.is_empty() {
            let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
            stack.setOrientation(objc2_app_kit::NSUserInterfaceLayoutOrientation::Vertical);
            stack.setAlignment(objc2_app_kit::NSLayoutAttribute::Leading);
            stack.setSpacing(6.0);
            let height = rows.len() as f64 * 42.0;
            stack.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, height)));
            alert.setAccessoryView(Some(&stack));
        }
        let this = tab.clone();
        tab.present(&alert, move |response| {
            // 1001: the second button.
            if response == 1001 {
                this.open_settings("settings#sitedata");
            }
        });
    });
}

/// The record name a trash button carries.
pub fn button_site(sender: &AnyObject) -> Option<String> {
    let b = sender.downcast_ref::<NSButton>()?;
    objc2_app_kit::NSUserInterfaceItemIdentification::identifier(b).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_belong_to_the_site_its_parents_and_subdomains() {
        assert!(belongs("github.com", "gist.github.com"));
        assert!(belongs("gist.github.com", "github.com"));
        assert!(belongs("GitHub.com", "github.com"));
        assert!(!belongs("hub.com", "github.com"), "not a suffix match");
        assert!(!belongs("example.com", "github.com"));
        assert_eq!(kind("WKWebsiteDataTypeIndexedDBDatabases"), "Databases");
        assert_eq!(kind("WKWebsiteDataTypeCookies"), "Cookies");
        assert_eq!(kind("_WKWebsiteDataTypeHSTSCache"), "Other");
    }
}
