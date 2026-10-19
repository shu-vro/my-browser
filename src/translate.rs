//! Translate Page (View menu): the page's text, translated on this Mac.
//!
//! resources/translate.js, in the browser's own script world, collects the
//! page's text and puts the translations back (and the original, for Show
//! Original). The translating is done by `my-browser-translate`
//! (helpers/translate.swift, built next to the browser by build.rs), a helper
//! process started on first use: Swift and Apple's Translation framework
//! never load into the browser itself. It's stopped after a minute without
//! work (`stop_idle_translator`, on the 30 s timer). Nothing goes over the
//! network: macOS translates on-device with the languages the user downloaded.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use objc2::DefinedClass;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSJSONReadingOptions, NSJSONSerialization, NSString};

use crate::App;
use crate::store::json_str;

/// What a translation request comes back with: (detected language, texts),
/// or (error code, message for the user).
pub type Outcome = Result<(String, Vec<String>), (String, String)>;
pub type Done = Box<dyn FnOnce(Outcome)>;

/// The running helper: where requests go, and which run of it this is.
pub struct Translator {
    stdin: ChildStdin,
    generation: u64,
    last_used: Instant,
}

#[derive(Default)]
pub struct State {
    helper: Option<Translator>,
    waiting: HashMap<u64, Done>,
    next_id: u64,
}

/// Texts sent per request: the first part of a page shows translated while
/// the rest is still being worked on.
pub const CHUNK: usize = 200;
/// The helper quits after this long without a request.
const IDLE: Duration = Duration::from_secs(60);

pub static TRANSLATE_JS: &str = include_str!("../resources/translate.js");

impl App {
    /// Translate `texts` into `to` (a language code; empty = the Mac's
    /// language). `done` runs on the main thread.
    pub fn translate(&self, texts: Vec<String>, to: &str, done: Done) {
        let mut state = self.ivars().translate.borrow_mut();
        if state.helper.is_none() {
            match self.start_translator(state.next_id) {
                Ok(helper) => state.helper = Some(helper),
                Err(e) => {
                    drop(state);
                    return done(Err(("missing".into(), e)));
                }
            }
        }
        state.next_id += 1;
        let id = state.next_id;
        let items: Vec<String> = texts.iter().map(|t| json_str(t)).collect();
        let line = format!("{{\"id\":{id},\"to\":{},\"texts\":[{}]}}\n", json_str(to), items.join(","));
        let helper = state.helper.as_mut().unwrap();
        helper.last_used = Instant::now();
        if helper.stdin.write_all(line.as_bytes()).and_then(|()| helper.stdin.flush()).is_err() {
            state.helper = None;
            drop(state);
            return done(Err(("failed".into(), "The translator stopped. Try again.".into())));
        }
        state.waiting.insert(id, done);
    }

    /// Start the helper, with a thread passing its replies to the main thread.
    fn start_translator(&self, generation: u64) -> Result<Translator, String> {
        let path = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("my-browser-translate")));
        let path = path.filter(|p| p.is_file()).ok_or("This build of my-browser can't translate pages.")?;
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Couldn't start the translator: {e}"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        // The delegate lives until the process exits, so the pointer stays valid.
        let app = self as *const App as usize;
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                crate::on_main(move || unsafe { &*(app as *const App) }.translation_arrived(&line));
            }
            let _ = child.wait();
            crate::on_main(move || unsafe { &*(app as *const App) }.translator_ended(generation));
        });
        Ok(Translator { stdin, generation, last_used: Instant::now() })
    }

    fn translation_arrived(&self, line: &str) {
        let Some((id, outcome)) = parse_reply(line) else { return };
        let done = self.ivars().translate.borrow_mut().waiting.remove(&id);
        if let Some(done) = done {
            done(outcome);
        }
    }

    /// The helper exited (stopped when idle, or it crashed): requests still
    /// waiting fail.
    fn translator_ended(&self, generation: u64) {
        let mut state = self.ivars().translate.borrow_mut();
        if state.helper.as_ref().is_some_and(|h| h.generation == generation) {
            state.helper = None;
        }
        let waiting: Vec<Done> = state.waiting.drain().map(|(_, d)| d).collect();
        drop(state);
        for done in waiting {
            done(Err(("failed".into(), "The translator stopped. Try again.".into())));
        }
    }

    /// Stop the helper after a minute without work (closing its input ends it).
    pub fn stop_idle_translator(&self) {
        let mut state = self.ivars().translate.borrow_mut();
        if state.waiting.is_empty() && state.helper.as_ref().is_some_and(|h| h.last_used.elapsed() >= IDLE) {
            state.helper = None;
        }
    }
}

/// `{"id", "from", "texts"}` or `{"id", "error", "message"}`.
fn parse_reply(line: &str) -> Option<(u64, Outcome)> {
    let data = NSData::with_bytes(line.as_bytes());
    let value = NSJSONSerialization::JSONObjectWithData_options_error(&data, NSJSONReadingOptions::empty()).ok()?;
    let dict = value.downcast_ref::<NSDictionary>()?;
    let get = |k: &str| dict.objectForKey(&NSString::from_str(k));
    let text = |k: &str| get(k).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string());
    let id = get("id")?.downcast::<objc2_foundation::NSNumber>().ok()?.unsignedLongLongValue();
    if let Some(error) = text("error") {
        return Some((id, Err((error, text("message").unwrap_or_default()))));
    }
    let texts: Retained<NSArray> = get("texts")?.downcast().ok()?;
    let texts = texts.iter().map(|t| t.downcast::<NSString>().map(|s| s.to_string()).unwrap_or_default()).collect();
    Some((id, Ok((text("from").unwrap_or_default(), texts))))
}

/// What resources/translate.js collected: a JSON array of strings.
pub fn parse_texts(json: &str) -> Vec<String> {
    let data = NSData::with_bytes(json.as_bytes());
    let Ok(value) = NSJSONSerialization::JSONObjectWithData_options_error(&data, NSJSONReadingOptions::empty()) else {
        return Vec::new();
    };
    let Ok(items) = value.downcast::<NSArray>() else { return Vec::new() };
    items.iter().map(|t| t.downcast::<NSString>().map(|s| s.to_string()).unwrap_or_default()).collect()
}

/// `translate_to`: empty (the Mac's language) or a language code like `en`,
/// `pt-BR` or `zh-Hans`.
pub fn valid_language(code: &str) -> bool {
    let mut parts = code.split('-');
    code.is_empty()
        || (parts.next().is_some_and(|l| (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase()))
            && parts.all(|p| (2..=4).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_replies_parse() {
        let (id, ok) = parse_reply(r#"{"id":3,"from":"fr","texts":["hello","a \"b\""]}"#).unwrap();
        assert_eq!((id, ok), (3, Ok(("fr".into(), vec!["hello".into(), "a \"b\"".into()]))));
        let (_, err) = parse_reply(r#"{"id":4,"error":"same","message":"Already English."}"#).unwrap();
        assert_eq!(err, Err(("same".into(), "Already English.".into())));
        assert!(parse_reply("garbage").is_none());
    }

    #[test]
    fn target_languages_are_codes() {
        for ok in ["", "en", "pt-BR", "zh-Hans", "yue"] {
            assert!(valid_language(ok), "{ok}");
        }
        for bad in ["EN", "e", "english", "en-", "en_US", "en-B R", "\"en"] {
            assert!(!valid_language(bad), "{bad}");
        }
    }
}
