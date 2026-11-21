//! Make the build-time knobs actually take effect when they change.
//!
//! `option_env!` is expanded while the crate is *compiled*, but cargo does not
//! fingerprint a crate by the environment the compiler saw — only by its
//! sources, features and profile. So setting `CS_SERVER_HOST` and rebuilding
//! silently reused the previous object file, and the APK kept whatever value
//! (or absence of one) the *first* build had. Every knob in this crate is read
//! that way, which is why `cargo clean -p cs_core` is repeated all through
//! `CLAUDE.md` as a manual step — a step that is only ever noticed when it is
//! forgotten.
//!
//! It was forgotten. The shipped versionCode 34 carries no server address at
//! all: `libmain.so` contains no dotted quad anywhere, so `redirect_target`
//! returned `None` and the unpatched guest dialled its own dead hardcoded host
//! instead of `cs_server`. The letter screen could never have connected.
//!
//! `rerun-if-env-changed` fixes it at the root: cargo re-runs this script when
//! one of those values changes, and a re-run dirties the crate, which
//! recompiles it — so `option_env!` sees the new value without a clean.
//!
//! The knob list is **scanned out of the sources** rather than written down
//! here. Hand-maintained knob tables have already been forgotten twice in this
//! repository (`CS_IME_H`/`CS_IME_DEBUG` shipped completely inert because they
//! were missing from `patch.rs`'s build-time list), and a second list would rot
//! the same way — silently, since a missing entry only costs a stale rebuild,
//! not an error. A scan cannot drift from what the code reads.
//!
//! std only, deliberately: a build-dependency here is a *host* crate, and host
//! build-deps are what drag the `dlltool` shim into the android cross-build
//! (see `build_android.ps1`). Nothing is worth reopening that.

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
