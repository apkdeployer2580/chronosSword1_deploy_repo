//! Make `cs_android`'s build-time knobs take effect when they change.
//!
//! Same job and same code as `cs_core/build.rs` — see that file for why this
//! exists at all. It is duplicated rather than shared because a shared crate
//! here would be a *host* build-dependency in the android cross-build, which is
//! what drags in the `dlltool` shim (`build_android.ps1`), and this is thirty
//! lines.
//!
//! `cs_core` got the script first and `cs_android` was left without one, so
//! `CS_SLOW_FRAME_MS` still had the stale-value trap: set it, rebuild, and
//! cargo reuses the object file compiled against the old value. It costs a
//! wrong number in one log line rather than a wrong APK, which is exactly why
//! nobody would notice.

use std::path::Path;

const MACRO: &str = "option_env!";

fn main() {
    // Emitting any `rerun-if-changed` narrows cargo to exactly these, so the
    // directory has to be listed for a newly added knob to be picked up: cargo
    // walks a directory path and notices additions inside it.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");

    let mut names = Vec::new();
    scan(Path::new("src"), &mut names);
    names.sort();
    names.dedup();
    for name in &names {
        println!("cargo:rerun-if-env-changed={name}");
    }
}

fn scan(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                harvest(&text, out);
            }
        }
    }
}

/// Every `option_env!("NAME")` in one file.
///
/// Whitespace is allowed between the `!`, the paren and the literal because
/// rustfmt puts it there once the surrounding expression gets long enough —
/// `potions.rs` already has one wrapped that way.
fn harvest(src: &str, out: &mut Vec<String>) {
    for (at, _) in src.match_indices(MACRO) {
        let rest = src[at + MACRO.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('(') else { continue };
        let rest = rest.trim_start();
        // Only a plain string literal is a knob name we can name to cargo. A
        // non-literal argument does not compile as `option_env!` anyway, so
        // skipping is not hiding anything.
        let Some(rest) = rest.strip_prefix('"') else { continue };
        let Some(end) = rest.find('"') else { continue };
        out.push(rest[..end].to_owned());
    }
}
