//! Build-time version stamping for the `koshell` binary (design 0024).
//!
//! koshell has no hand-maintained release number yet, and the question a version string
//! has to answer here is "which build is this?" — the three artifacts a user can be
//! running (this binary, the koshell wrapping their terminal, the daemon) are replaced
//! independently, so a constant carried in `Cargo.toml` would report the same `0.1.0` for
//! all of them forever. The version is therefore resolved when the binary is built, in
//! this order:
//!
//! 1. `KOSHELL_VERSION` — an explicit version for a release build
//!    (`make KOSHELL_VERSION=1.2.0`). The Makefile resolves the same chain once and
//!    exports it, so a single `make` never stamps two different build times into koshell
//!    and the daemon.
//! 2. The tag on `HEAD` (`git describe --tags --exact-match`), with a leading `v`
//!    stripped: a tagged checkout stamps its release without being told.
//! 3. The build's UTC timestamp, `YYYYMMDD.HHMMSS` — sortable, filename- and tag-safe,
//!    and comparable across machines, which a local-time stamp would not be.
//!
//! `packages/ai-daemon/scripts/version.ts` implements the same chain for the daemon; the
//! two are kept in step by the design doc rather than by shared code, since one is a
//! build script for cargo and the other for bun.
//!
//! Both lookups shell out rather than pull in a date or git crate: this crate is Unix-only
//! (it already requires `/bin/sh` at runtime to spawn the daemon), `date -u` and `git` are
//! exactly the tools that define the two formats, and a hand-rolled civil-date conversion
//! would be arithmetic no test in this package can reach — build scripts are not covered by
//! `cargo test`.

use std::process::Command;

fn main() {
    // Emitting any rerun-if directive replaces cargo's default "re-run when any file in the
    // package changed", so the file rungs have to be spelled out: without them an edited
    // source file would keep the first build's timestamp for the life of the target dir.
    println!("cargo:rerun-if-env-changed=KOSHELL_VERSION");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rustc-env=KOSHELL_BUILD_VERSION={}", resolve());
}

/// The version to stamp into this build: explicit, then the tag on `HEAD`, then the build
/// timestamp. The package version is the last resort, reached only if `date` itself is
/// unavailable — a wrong-looking version is better than a failed build.
fn resolve() -> String {
    if let Some(explicit) = std::env::var("KOSHELL_VERSION").ok().and_then(clean) {
        return explicit;
    }
    if let Some(tag) = head_tag() {
        return tag;
    }
    if let Some(stamp) = utc_stamp() {
        return stamp;
    }
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string())
}

/// The exact tag on `HEAD`, with a leading `v` stripped. `None` outside a repository, on an
/// untagged commit, or without git — every one of which is an ordinary build, not an error.
fn head_tag() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let output = Command::new("git")
        .args(["-C", &manifest_dir, "describe", "--tags", "--exact-match"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tag = clean(String::from_utf8(output.stdout).ok()?)?;
    Some(tag.strip_prefix('v').unwrap_or(&tag).to_string())
}

/// This build's UTC timestamp as `YYYYMMDD.HHMMSS`.
fn utc_stamp() -> Option<String> {
    let output = Command::new("date")
        .args(["-u", "+%Y%m%d.%H%M%S"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    clean(String::from_utf8(output.stdout).ok()?)
}

/// The first non-empty line of `value`, trimmed. A version string reaches a `--version`
/// line and a `cargo:rustc-env` directive, neither of which survives an embedded newline,
/// so anything past the first line is dropped rather than trusted.
fn clean(value: String) -> Option<String> {
    let first = value.lines().next()?.trim();
    (!first.is_empty()).then(|| first.to_string())
}
