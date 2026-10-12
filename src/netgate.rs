//! The network gate: the only place the browser's own code opens a
//! connection, and only to an address the user allowed.
//!
//! WebKit makes extensions' requests itself, but some services refuse them
//! (WebKit sends its own `webkit-extension://` Origin and ignores the
//! extension's header rules). Extensions adapted for `offscreen` (see
//! `extensions::adapt_offscreen`) get resources/websocket.js in their pages:
//! their `WebSocket` asks the browser to connect over a native port instead.
//!
//! - Only `wss:`, only to a host the extension's manifest asks for, never to
//!   this Mac or the local network.
//! - Blocked until allowed in Settings → Network. Blocked requests are listed
//!   for this session only; not acting on one means it stays denied.
//! - An allowed rule is kept: an address without its query
//!   (`wss://host/path`), or a whole host (`wss://host/*`).
//! - No cookies or cache kept (an ephemeral session), no Origin sent; the
//!   User-Agent is the extension's own (see `host_extension_audio`).

use std::cell::{OnceCell, RefCell};
use std::net::{IpAddr, Ipv4Addr};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, Message, define_class, msg_send};
use objc2_foundation::{
    NSData, NSDataBase64DecodingOptions, NSDataBase64EncodingOptions, NSDictionary, NSError, NSMutableURLRequest,
    NSNumber, NSObject, NSObjectProtocol, NSOperationQueue, NSString, NSURL, NSURLSession, NSURLSessionConfiguration,
    NSURLSessionDelegate, NSURLSessionTask, NSURLSessionTaskDelegate, NSURLSessionWebSocketCloseCode,
    NSURLSessionWebSocketDelegate, NSURLSessionWebSocketMessage, NSURLSessionWebSocketTask,
};
use objc2_web_kit::{WKWebExtensionContext, WKWebExtensionMessagePort};

use crate::App;
use crate::store::json_str;

/// A request blocked this session, as Settings → Network lists it.
pub struct Blocked {
    pub ext: i64,
    pub name: String,
    pub address: String,
    /// The user said no. Undecided ones are blocked too, but still offered.
    pub denied: bool,
    pub count: u32,
}

/// `wss://host[:port]/path`: scheme and host lowercase, no query or
/// fragment. None for anything but a plain `wss:` address.
pub fn address_of(url: &str) -> Option<String> {
    if !url.get(..6)?.eq_ignore_ascii_case("wss://") {
        return None;
    }
    let rest = &url[6..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest[..end].to_ascii_lowercase();
    if authority.is_empty() || authority.contains(['@', '\\', ' ', '%']) {
        return None;
    }
    let tail = &rest[end..];
    let path = &tail[..tail.find(['?', '#']).unwrap_or(tail.len())];
    Some(format!("wss://{authority}{}", if path.is_empty() { "/" } else { path }))
}

/// The rule allowing every address on `address`'s host.
pub fn site_rule(address: &str) -> String {
    let rest = &address[6..];
    format!("wss://{}/*", &rest[..rest.find('/').unwrap_or(rest.len())])
}

pub fn allows(rule: &str, address: &str) -> bool {
    match rule.strip_suffix('*') {
        Some(prefix) => address.starts_with(prefix),
        None => rule == address,
    }
}

/// This Mac or the local network, by name or address. Hosts that only look
/// numeric (`0x7f.1`, `2130706433`) count too: the resolver may read them as
/// addresses. A public name can still point here: `resolves_local` checks
/// what it resolves to before connecting.
pub fn is_local(address: &str) -> bool {
    let host = host_of(address);
    match host.parse::<IpAddr>() {
        Ok(ip) => local_ip(ip),
        Err(_) => {
            let last = host.rsplit('.').next().unwrap_or("");
            let numeric = !last.is_empty() && (last.bytes().all(|c| c.is_ascii_digit()) || last.starts_with("0x"));
            numeric || !host.contains('.') || host.ends_with(".localhost") || host.ends_with(".local")
        }
    }
}

/// `wss://host[:port]/…` → `host`, without brackets or a trailing dot.
fn host_of(address: &str) -> &str {
    let authority = &address[6..address[6..].find('/').map_or(address.len(), |i| i + 6)];
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.trim_end_matches('.')
}

fn local_ip(ip: IpAddr) -> bool {
    let local_v4 = |ip: Ipv4Addr| {
        let [a, b, ..] = ip.octets();
        ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast()
            || (a == 100 && (64..128).contains(&b))
    };
    match ip {
        IpAddr::V4(ip) => local_v4(ip),
        IpAddr::V6(ip) => {
            let s = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || s & 0xfe00 == 0xfc00
                || s & 0xffc0 == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(local_v4)
        }
    }
}

/// Does `address`'s name resolve to this Mac or the local network? A name
/// that doesn't resolve isn't: connecting fails on its own. Blocks on DNS:
/// call it off the main thread.
// ponytail: the system resolves again when connecting, so a name with a
// zero TTL can still switch to a local address in between (DNS rebinding).
// This stops names that simply point here (`localtest.me`); closing the gap
// needs connecting to the checked address, which NSURLSession can't do.
pub fn resolves_local(address: &str) -> bool {
    use std::net::ToSocketAddrs;
    let authority = address[6..].split('/').next().unwrap_or("");
    let port = authority.rsplit_once(':').filter(|(_, p)| !p.contains(']')).and_then(|(_, p)| p.parse().ok());
    (host_of(address), port.unwrap_or(443)).to_socket_addrs().is_ok_and(|mut a| a.any(|a| local_ip(a.ip())))
}

/// Resolve `address` on another thread, then call `done(local)` on the main
/// thread.
fn after_resolving(address: String, done: Box<dyn FnOnce(bool)>) {
    let done = crate::MainOnly(done);
    std::thread::spawn(move || {
        let done = done; // the whole wrapper, not its field
        let local = resolves_local(&address);
        crate::on_main(move || (done.0)(local));
    });
}

/// Open connections: the page's port and its socket. Main thread only (the
/// session delivers everything on the main queue).
struct Gate {
    session: Retained<NSURLSession>,
    conns: RefCell<Vec<(Retained<WKWebExtensionMessagePort>, Retained<NSURLSessionWebSocketTask>)>>,
}

thread_local! {
    /// Made on the first allowed connection; nothing exists before that.
    static GATE: OnceCell<Gate> = const { OnceCell::new() };
}

/// Run `f` on the gate, if it exists.
fn with_gate<R>(f: impl FnOnce(&Gate) -> R) -> Option<R> {
    GATE.with(|g| g.get().map(f))
}

define_class!(
    /// The URL session's delegate. Holds nothing: callbacks arrive on the
    /// main queue and use `GATE`.
    #[unsafe(super(NSObject))]
    struct GateDelegate;

    unsafe impl NSObjectProtocol for GateDelegate {}
    unsafe impl NSURLSessionDelegate for GateDelegate {}

    unsafe impl NSURLSessionTaskDelegate for GateDelegate {
        /// Failed before or after opening (refused, DNS, TLS, dropped).
        #[unsafe(method(URLSession:task:didCompleteWithError:))]
        fn did_complete(&self, _s: &NSURLSession, task: &NSURLSessionTask, error: Option<&NSError>) {
            if let Some(port) = with_gate(|g| g.take(task)).flatten() {
                if error.is_some() {
                    send(&port, r#"{"t":"error"}"#);
                }
                send(&port, r#"{"t":"close","code":1006,"reason":""}"#);
            }
        }
    }

    unsafe impl NSURLSessionWebSocketDelegate for GateDelegate {
        #[unsafe(method(URLSession:webSocketTask:didOpenWithProtocol:))]
        fn did_open(&self, _s: &NSURLSession, task: &NSURLSessionWebSocketTask, protocol: Option<&NSString>) {
            if let Some(port) = with_gate(|g| g.port_of(task)).flatten() {
                let protocol = protocol.map(|p| p.to_string()).unwrap_or_default();
                send(&port, &format!(r#"{{"t":"open","protocol":{}}}"#, json_str(&protocol)));
            }
        }

        #[unsafe(method(URLSession:webSocketTask:didCloseWithCode:reason:))]
        fn did_close(
            &self,
            _s: &NSURLSession,
            task: &NSURLSessionWebSocketTask,
            code: NSURLSessionWebSocketCloseCode,
            reason: Option<&NSData>,
        ) {
            if let Some(port) = with_gate(|g| g.take(task)).flatten() {
                let reason = reason.map(|r| String::from_utf8_lossy(&r.to_vec()).into_owned()).unwrap_or_default();
                send(&port, &format!(r#"{{"t":"close","code":{},"reason":{}}}"#, code.0, json_str(&reason)));
            }
        }
    }
);

fn send(port: &WKWebExtensionMessagePort, json: &str) {
    unsafe { port.sendMessage_completionHandler(Some(&NSString::from_str(json)), None) };
}

fn same(a: &NSURLSessionTask, b: &NSURLSessionTask) -> bool {
    std::ptr::eq(a, b)
}

impl Gate {
    fn new() -> Gate {
        let config = NSURLSessionConfiguration::ephemeralSessionConfiguration();
        config.setHTTPShouldSetCookies(false);
        let delegate: Retained<GateDelegate> = unsafe { msg_send![GateDelegate::alloc(), init] };
        let session = unsafe {
            NSURLSession::sessionWithConfiguration_delegate_delegateQueue(
                &config,
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(&NSOperationQueue::mainQueue()),
            )
        };
        Gate { session, conns: RefCell::new(Vec::new()) }
    }

    fn port_of(&self, task: &NSURLSessionTask) -> Option<Retained<WKWebExtensionMessagePort>> {
        self.conns.borrow().iter().find(|(_, t)| same(t, task)).map(|(p, _)| p.clone())
    }

    fn task_of(&self, port: &WKWebExtensionMessagePort) -> Option<Retained<NSURLSessionWebSocketTask>> {
        self.conns.borrow().iter().find(|(p, _)| std::ptr::eq(&**p, port)).map(|(_, t)| t.clone())
    }

    /// Forget a finished connection; its port, if it was still open.
    fn take(&self, task: &NSURLSessionTask) -> Option<Retained<WKWebExtensionMessagePort>> {
        let mut conns = self.conns.borrow_mut();
        let i = conns.iter().position(|(_, t)| same(t, task))?;
        Some(conns.remove(i).0)
    }

    fn open(&self, port: &WKWebExtensionMessagePort, url: &NSURL, user_agent: Option<&str>, protocols: &str) {
        let request = NSMutableURLRequest::requestWithURL(url);
        if let Some(ua) = user_agent {
            request.setValue_forHTTPHeaderField(Some(&NSString::from_str(ua)), &NSString::from_str("User-Agent"));
        }
        if !protocols.is_empty() {
            request.setValue_forHTTPHeaderField(
                Some(&NSString::from_str(protocols)),
                &NSString::from_str("Sec-WebSocket-Protocol"),
            );
        }
        let task = self.session.webSocketTaskWithRequest(&request);
        self.conns.borrow_mut().push((port.retain(), task.clone()));
        task.resume();
        receive(task);
    }

    fn send_to(&self, port: &WKWebExtensionMessagePort, message: &NSURLSessionWebSocketMessage) {
        if let Some(task) = self.task_of(port) {
            let done = RcBlock::new(|_e: *mut NSError| {});
            unsafe { task.sendMessage_completionHandler(message, &done) };
        }
    }

    fn close(&self, port: &WKWebExtensionMessagePort, code: isize, reason: &str) {
        if let Some(task) = self.task_of(port) {
            let reason = NSData::with_bytes(reason.as_bytes());
            task.cancelWithCloseCode_reason(NSURLSessionWebSocketCloseCode(code), Some(&reason));
        }
    }

    /// The page went away: drop its socket.
    fn drop_port(&self, port: &WKWebExtensionMessagePort) {
        let mut conns = self.conns.borrow_mut();
        if let Some(i) = conns.iter().position(|(p, _)| std::ptr::eq(&**p, port)) {
            let (_, task) = conns.remove(i);
            task.cancel();
        }
    }
}

/// Pass every incoming message to the page, until the socket ends.
fn receive(task: Retained<NSURLSessionWebSocketTask>) {
    let t = task.clone();
    let block = RcBlock::new(move |message: *mut NSURLSessionWebSocketMessage, _e: *mut NSError| {
        let Some(message) = (unsafe { message.as_ref() }) else { return };
        let Some(port) = with_gate(|g| g.port_of(&t)).flatten() else { return };
        let json = match (message.string(), message.data()) {
            (Some(text), _) => format!(r#"{{"t":"message","text":{}}}"#, json_str(&text.to_string())),
            (None, Some(data)) => {
                let b64 = data.base64EncodedStringWithOptions(NSDataBase64EncodingOptions::empty());
                format!(r#"{{"t":"message","b64":"{b64}"}}"#)
            }
            _ => return,
        };
        send(&port, &json);
        receive(t.clone());
    });
    unsafe { task.receiveMessageWithCompletionHandler(&block) };
}

fn string_of(dict: &NSDictionary, key: &str) -> Option<String> {
    let value = dict.objectForKey(&NSString::from_str(key))?;
    value.downcast::<NSString>().ok().map(|s| s.to_string())
}

impl App {
    /// `runtime.connectNative` from an extension page's WebSocket stand-in.
    /// Each port is one socket: `open`, then `send`s, then `close`.
    pub fn net_connect(&self, port: &WKWebExtensionMessagePort, ctx: &WKWebExtensionContext) {
        let Some(ext) = self.extension_id_of(ctx) else { return unsafe { port.disconnect() } };
        let (this, p, c) = (self.retain(), port.retain(), ctx.retain());
        let on_message = RcBlock::new(move |message: *mut AnyObject, _e: *mut NSError| {
            if let Some(dict) = unsafe { message.as_ref() }.and_then(|m| m.downcast_ref::<NSDictionary>()) {
                this.net_message(ext, &c, &p, dict);
            }
        });
        let p = port.retain();
        let on_disconnect = RcBlock::new(move |_e: *mut NSError| {
            with_gate(|g| g.drop_port(&p));
            // The handlers hold the port: let it go.
            unsafe {
                p.setMessageHandler(None);
                p.setDisconnectHandler(None);
            }
        });
        unsafe {
            port.setMessageHandler(Some(&on_message));
            port.setDisconnectHandler(Some(&on_disconnect));
        }
    }

    fn net_message(&self, ext: i64, ctx: &WKWebExtensionContext, port: &WKWebExtensionMessagePort, msg: &NSDictionary) {
        match string_of(msg, "t").as_deref() {
            Some("open") => {
                let url = string_of(msg, "url").unwrap_or_default();
                if with_gate(|g| g.task_of(port).is_some()) == Some(true) {
                    return;
                }
                if let Err(why) = self.net_check(ext, ctx, &url) {
                    send(port, r#"{"t":"error"}"#);
                    send(port, &format!(r#"{{"t":"close","code":1006,"reason":{}}}"#, json_str(why)));
                    return;
                }
                let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(&url)) else { return };
                let protocols = string_of(msg, "protocols").unwrap_or_default();
                let ua = crate::extensions::background_user_agent(ctx);
                let port = port.retain();
                let address = address_of(&url).unwrap_or_default();
                after_resolving(
                    address,
                    Box::new(move |local| {
                        if local {
                            send(&port, r#"{"t":"error"}"#);
                            let why = "my-browser: this Mac and the local network are blocked";
                            send(&port, &format!(r#"{{"t":"close","code":1006,"reason":{}}}"#, json_str(why)));
                            return;
                        }
                        GATE.with(|g| g.get_or_init(Gate::new).open(&port, &nsurl, ua.as_deref(), &protocols));
                    }),
                );
            }
            Some("send") => {
                let message = if let Some(text) = string_of(msg, "text") {
                    NSURLSessionWebSocketMessage::initWithString(
                        NSURLSessionWebSocketMessage::alloc(),
                        &NSString::from_str(&text),
                    )
                } else if let Some(data) = string_of(msg, "b64").and_then(|b| {
                    NSData::initWithBase64EncodedString_options(
                        NSData::alloc(),
                        &NSString::from_str(&b),
                        NSDataBase64DecodingOptions::empty(),
                    )
                }) {
                    NSURLSessionWebSocketMessage::initWithData(NSURLSessionWebSocketMessage::alloc(), &data)
                } else {
                    return;
                };
                with_gate(|g| g.send_to(port, &message));
            }
            Some("close") => {
                let code = msg
                    .objectForKey(&NSString::from_str("code"))
                    .and_then(|c| c.downcast::<NSNumber>().ok())
                    .map_or(1000, |n| n.integerValue());
                with_gate(|g| g.close(port, code, &string_of(msg, "reason").unwrap_or_default()));
            }
            _ => {}
        }
    }

    /// May `ext` connect to `url`? Err says why not. A refusal the user can
    /// change (not allowed yet) is listed in Settings → Network.
    fn net_check(&self, ext: i64, ctx: &WKWebExtensionContext, url: &str) -> Result<(), &'static str> {
        let address = address_of(url).ok_or("my-browser: only wss: addresses")?;
        if is_local(&address) {
            return Err("my-browser: this Mac and the local network are blocked");
        }
        // WebKit rejects `wss:` match patterns, so the extension's access to
        // the host is its `https:` one (Chrome treats them alike).
        let https = format!("https://{}", &url[6..]);
        let nsurl = NSURL::URLWithString(&NSString::from_str(&https)).ok_or("my-browser: bad address")?;
        let asked = unsafe { ctx.webExtension().allRequestedMatchPatterns() }
            .iter()
            .any(|p| unsafe { p.matchesURL(Some(&nsurl)) });
        if !asked {
            return Err("my-browser: not a site this extension asks for");
        }
        if self.store().net_allowed().iter().any(|(e, rule)| *e == ext && allows(rule, &address)) {
            return Ok(());
        }
        let mut blocked = self.ivars().net_blocked.borrow_mut();
        match blocked.iter_mut().find(|b| b.ext == ext && b.address == address) {
            Some(b) => b.count += 1,
            None => {
                let name = self.extension_meta(ext).map(|e| crate::extensions::extension_name(&e)).unwrap_or_default();
                blocked.push(Blocked { ext, name, address, denied: false, count: 1 });
            }
        }
        drop(blocked);
        self.refresh_extension_buttons();
        Err("my-browser: blocked until allowed in Settings → Network")
    }

    /// Requests waiting for an answer (the toolbar shows a button).
    pub fn net_waiting(&self) -> bool {
        self.ivars().net_blocked.borrow().iter().any(|b| !b.denied)
    }

    /// Settings → Network: `{"blocked":[…],"allowed":[…]}`.
    pub fn net_json(&self) -> String {
        let name =
            |ext: i64| self.extension_meta(ext).map(|e| crate::extensions::extension_name(&e)).unwrap_or_default();
        let blocked: Vec<String> = self
            .ivars()
            .net_blocked
            .borrow()
            .iter()
            .map(|b| {
                format!(
                    r#"{{"ext":{},"name":{},"address":{},"site":{},"denied":{},"count":{}}}"#,
                    b.ext,
                    json_str(&b.name),
                    json_str(&b.address),
                    json_str(&site_rule(&b.address)),
                    b.denied,
                    b.count
                )
            })
            .collect();
        let allowed: Vec<String> = self
            .store()
            .net_allowed()
            .iter()
            .map(|(ext, rule)| format!(r#"{{"ext":{ext},"name":{},"rule":{}}}"#, json_str(&name(*ext)), json_str(rule)))
            .collect();
        format!(r#"{{"blocked":[{}],"allowed":[{}]}}"#, blocked.join(","), allowed.join(","))
    }

    /// Allow `rule` for `ext`: the address or the site of a request blocked
    /// this session (nothing else can be allowed). Matching requests leave
    /// the blocked list.
    pub fn net_allow(&self, ext: i64, rule: &str) -> Result<(), String> {
        let mut blocked = self.ivars().net_blocked.borrow_mut();
        if !blocked.iter().any(|b| b.ext == ext && (b.address == rule || site_rule(&b.address) == rule)) {
            return Err("Only a blocked request's address or site can be allowed.".into());
        }
        self.store().allow_net(ext, rule);
        blocked.retain(|b| !(b.ext == ext && allows(rule, &b.address)));
        drop(blocked);
        self.refresh_extension_buttons();
        Ok(())
    }

    pub fn net_deny(&self, ext: i64, address: &str) {
        for b in self.ivars().net_blocked.borrow_mut().iter_mut().filter(|b| b.ext == ext && b.address == address) {
            b.denied = true;
        }
        self.refresh_extension_buttons();
    }

    pub fn net_forget(&self, ext: i64, rule: &str) {
        self.store().forget_net(ext, rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_drop_the_query_and_rules_match() {
        let a = address_of("WSS://Speech.Example.com/v1/edge?token=1&id=2#x").unwrap();
        assert_eq!(a, "wss://speech.example.com/v1/edge");
        assert_eq!(address_of("wss://h.example?x=1").unwrap(), "wss://h.example/");
        assert_eq!(site_rule(&a), "wss://speech.example.com/*");
        assert!(allows("wss://speech.example.com/v1/edge", &a));
        assert!(allows("wss://speech.example.com/*", &a));
        assert!(!allows("wss://speech.example.com/v1/edge", "wss://speech.example.com/v1/edge2"));
        assert!(!allows("wss://example.com/*", "wss://example.com.evil.net/x"));
        assert!(!allows("wss://example.com/*", "wss://example.com:8443/x"));
        for bad in ["ws://h.example/", "https://h.example/", "wss://user@h.example/", "wss://", "wss://h.example\\x/"] {
            assert_eq!(address_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn this_mac_and_the_local_network_are_local() {
        for local in [
            "wss://localhost/",
            "wss://a.localhost/",
            "wss://printer.local/",
            "wss://router/",
            "wss://127.0.0.1/",
            "wss://10.1.2.3:8443/",
            "wss://192.168.1.1/",
            "wss://169.254.169.254/",
            "wss://100.64.0.1/",
            "wss://[::1]/",
            "wss://[fe80::1]/",
            "wss://[fd00::1]/",
            "wss://[::ffff:127.0.0.1]/",
            "wss://0.0.0.0/",
            "wss://2130706433/",
            "wss://0x7f.1/",
            "wss://127.1/",
            "wss://localhost./",
        ] {
            assert!(is_local(local), "{local}");
        }
        for public in
            ["wss://speech.platform.bing.com/", "wss://8.8.8.8/", "wss://[2001:4860::8888]/", "wss://a.b.example:443/"]
        {
            assert!(!is_local(public), "{public}");
        }
    }

    #[test]
    fn names_are_checked_by_what_they_resolve_to() {
        // No network needed: these resolve locally or are literal addresses.
        assert!(resolves_local("wss://localhost:9/"));
        assert!(resolves_local("wss://127.0.0.1/"));
        assert!(!resolves_local("wss://does-not-resolve.invalid/"), "fails as a connection instead");
        assert!(!resolves_local("wss://8.8.8.8/"));
        assert!(!resolves_local("wss://[2001:4860::8888]:8443/x"));
    }
}
