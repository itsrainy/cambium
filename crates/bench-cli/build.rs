//! Stamps the short git sha of the checkout into `CAMBIUM_GIT_SHA`, so every
//! result file names the cambium that wrote it without shelling out at run
//! time — a run's working directory is a *subject's* checkout, whose HEAD is
//! not ours.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CAMBIUM_GIT_SHA={sha}");
    // Rebuild when HEAD moves. `--git-path` resolves worktrees, where `.git`
    // is a file pointing elsewhere.
    for name in ["HEAD", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", name]) {
            let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(p);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
    if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(p) = git(&["rev-parse", "--git-path", &head]) {
            let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(p);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
}
