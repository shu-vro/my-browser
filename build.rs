//! Builds the Translate Page helper (helpers/translate.swift) and puts it
//! next to the browser binary, where the browser looks for it. It's a
//! separate process so Swift and the Translation framework only load when a
//! page is translated, never at launch (see src/translate.rs).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=helpers/translate.swift");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR is target/[<triple>/]<profile>/build/<package>-<hash>/out.
    let Some(dir) = out.ancestors().nth(3) else { return };
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x86_64",
        _ => "arm64",
    };
    let built = Command::new("xcrun")
        .args(["swiftc", "-O", "-parse-as-library", "-target", &format!("{arch}-apple-macos15.4")])
        .arg("helpers/translate.swift")
        .arg("-o")
        .arg(dir.join("my-browser-translate"))
        .status();
    if !built.is_ok_and(|s| s.success()) {
        println!(
            "cargo:warning=Couldn't build the translation helper (needs Xcode's swiftc); Translate Page won't work."
        );
    }
}
