//! The shell in the CI templates, run.
//!
//! `docs/ci/*.yml` and `docs/CI.md` are copied into other repositories as
//! they are, so their `run:` blocks are code a caller runs without reading.
//! These tests run them against stub `cambi`, `bazel`, `curl` and `sleep`
//! binaries on `PATH`, the way GitHub runs a `run:` step (`bash -e`, no
//! `pipefail`) or a plain CI runs a script (`sh`).

use std::path::{Path, PathBuf};
use std::process::Command;

fn docs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(docs().join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

/// The `run: |` block of the step named `name` in a workflow, dedented.
fn run_block(yml: &str, name: &str) -> String {
    let mut lines = yml
        .lines()
        .skip_while(|l| l.trim() != format!("- name: {name}"));
    lines
        .next()
        .unwrap_or_else(|| panic!("no step named {name}"));
    let mut lines = lines.skip_while(|l| l.trim() != "run: |");
    let run = lines
        .next()
        .unwrap_or_else(|| panic!("{name} has no run block"));
    let indent = run.len() - run.trim_start().len();
    let body: Vec<&str> = lines
        .take_while(|l| l.trim().is_empty() || l.len() - l.trim_start().len() > indent)
        .collect();
    let strip = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    body.iter()
        .map(|l| l.get(strip..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every fenced block of `lang` in a Markdown file that contains `needle`.
fn fenced(md: &str, lang: &str, needle: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<Vec<&str>> = None;
    for l in md.lines() {
        match &mut cur {
            None if l.trim() == format!("```{lang}") => cur = Some(Vec::new()),
            Some(b) if l.trim() == "```" => {
                let text = b.join("\n");
                if text.contains(needle) {
                    out.push(text);
                }
                cur = None;
            }
            Some(b) => b.push(l),
            None => {}
        }
    }
    out
}

/// A directory of stub binaries, each of which appends its argv to
/// `<dir>/<name>.log` and then runs `tail` (its exit, usually).
struct Stubs {
    dir: tempfile::TempDir,
}

impl Stubs {
    fn new(stubs: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, tail) in stubs {
            let log = dir.path().join(format!("{name}.log"));
            cambium_testkit::executable_script(
                &dir.path().join(name),
                &format!("echo \"$@\" >> '{}'\n{tail}", log.display()),
            );
        }
        Stubs { dir }
    }

    fn calls(&self, name: &str) -> usize {
        std::fs::read_to_string(self.dir.path().join(format!("{name}.log")))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Runs `script` under `shell` (with its flags) in the stub directory,
    /// with the stubs first on `PATH`. Returns the exit code and stderr.
    fn run(&self, shell: &[&str], script: &str, env: &[(&str, &str)]) -> (i32, String) {
        let path = format!(
            "{}:{}",
            self.dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(shell[0]);
        cmd.args(&shell[1..])
            .arg("-c")
            .arg(script)
            .current_dir(self.dir.path())
            .env("PATH", path)
            .env("HOME", self.dir.path())
            .env("GITHUB_PATH", self.dir.path().join("github_path"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("the shell runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// A store that stays down (exit 5 every time) is tried three times with a
/// pause between tries, not after the last one: the last pause only delays
/// the `bazel test //...` that follows.
#[test]
fn the_retry_loops_do_not_sleep_after_the_last_try() {
    let ci = read("CI.md");
    let mut scripts = vec![(
        "docs/ci/affected-tests.yml",
        run_block(&read("ci/affected-tests.yml"), "Affected tests"),
        &["bash", "-e"][..],
    )];
    for s in fenced(&ci, "sh", "for try in") {
        scripts.push(("docs/CI.md", s, &["sh", "-e"][..]));
    }
    assert!(
        scripts.len() >= 3,
        "the workflow's loop, CI.md's copy of it and the generic script"
    );
    for (from, script, shell) in scripts {
        let stubs = Stubs::new(&[
            ("cambi", "exit 5"),
            ("sleep", "exit 0"),
            ("bazel", "exit 0"),
        ]);
        let (code, stderr) = stubs.run(shell, &script, &[("BASE", "HEAD~1")]);
        assert_eq!(code, 0, "{from}:\n{script}\n{stderr}");
        assert_eq!(stubs.calls("cambi"), 3, "{from}: three tries:\n{script}");
        assert_eq!(
            stubs.calls("sleep"),
            2,
            "{from}: a pause between tries and none after the last:\n{script}"
        );
        assert_eq!(stubs.calls("bazel"), 1, "{from}: then `bazel test //...`");
    }
}

/// `curl -fsSL URL | sh` exits 0 when the download fails, because `sh` reads
/// an empty script and runs it. The Install step must fail there, not pass
/// and leave the next step to find no `cambi`.
#[test]
fn the_install_steps_fail_when_the_download_does() {
    for yml in ["ci/affected-tests.yml", "ci/cambium-index.yml"] {
        let script = run_block(&read(yml), "Install cambi");
        assert!(script.contains("curl"), "{yml}:\n{script}");
        // curl -f on a 404: nothing on stdout, exit 22.
        let stubs = Stubs::new(&[("curl", "echo 'curl: (22) 404' >&2\nexit 22")]);
        let (code, stderr) = stubs.run(&["bash", "-e"], &script, &[("CAMBI_VERSION", "v0.1.0")]);
        assert_ne!(
            code, 0,
            "docs/{yml}: the Install step passed with the download failing:\n{script}\n{stderr}"
        );
        assert!(
            !stubs.dir.path().join("github_path").exists(),
            "docs/{yml}: nothing is added to PATH after a failed install"
        );
    }
}
