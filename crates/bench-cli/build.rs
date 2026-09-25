//! Stamps the short git sha cambium was built from into `CAMBIUM_GIT_SHA`, so
//! every result file names the cambium that wrote it without shelling out at
//! run time (a run's working directory is a *subject's* checkout, whose HEAD
//! is not ours).
//!
//! The sha comes from the first of these that answers:
//!
//! 1. `CAMBIUM_GIT_SHA` in the build environment, for a build from a source
//!    tree with no git metadata at all.
//! 2. `.cargo_vcs_info.json` beside the manifest. `cargo package` writes it,
//!    so it exists only in packaged sources (`cargo install cambium-cli`, a
//!    `cargo publish` verify), and it names the commit that was packaged.
//! 3. `git rev-parse HEAD`, accepted only when the repository's top level is
//!    this workspace's root. This covers a clone, a worktree, and
//!    `cargo install --git`, whose checkout keeps its `.git` and has no
//!    `.cargo_vcs_info.json`. Without the check a crate unpacked under a
//!    directory that is itself some other repository (a dotfiles `$HOME`, a
//!    tarball inside another checkout) would stamp that repository's sha.
//! 4. `unknown`.
//!
//! The sha is hex and nothing else: `cambium-bench --require-sha` parses the
//! `(sha)` in `cambi --version` and accepts only hex digits, so there is no
//! `-dirty` suffix (and build.rs does not rerun on working-tree edits, so one
//! would go stale anyway).
//!
//! crates/cli, crates/bench and crates/bench-cli carry the same build.rs;
//! change them together.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Characters kept from a full sha, matching `git rev-parse --short`'s default.
const SHORT: usize = 7;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(manifest_dir())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// A usable sha: at least four hex digits, cut to `SHORT`.
fn short_hex(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() >= 4 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| s[..s.len().min(SHORT)].to_ascii_lowercase())
}

fn from_env() -> Option<String> {
    println!("cargo:rerun-if-env-changed=CAMBIUM_GIT_SHA");
    short_hex(&std::env::var("CAMBIUM_GIT_SHA").ok()?)
}

/// `{"git": {"sha1": "…"}, "path_in_vcs": "…"}`, read without a JSON
/// dependency: the value after the `"sha1"` key.
fn from_vcs_info() -> Option<String> {
    let path = manifest_dir().join(".cargo_vcs_info.json");
    let text = std::fs::read_to_string(&path).ok()?;
    println!("cargo:rerun-if-changed={}", path.display());
    let after_key = text.split_once("\"sha1\"")?.1;
    let value = after_key.split_once('"')?.1.split_once('"')?.0;
    short_hex(value)
}

fn from_git() -> Option<String> {
    let top = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    // Every crate sits at <workspace>/crates/<name>.
    let root = manifest_dir().join("../..");
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same(&top, &root) {
        return None;
    }
    // Rebuild when HEAD moves. `--git-path` resolves worktrees, where `.git`
    // is a file pointing elsewhere.
    let mut watch = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(head);
    }
    for name in &watch {
        if let Some(p) = git(&["rev-parse", "--git-path", name]) {
            let p = manifest_dir().join(p);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
    short_hex(&git(&["rev-parse", "HEAD"])?)
}

fn main() {
    let sha = from_env()
        .or_else(from_vcs_info)
        .or_else(from_git)
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CAMBIUM_GIT_SHA={sha}");
}
