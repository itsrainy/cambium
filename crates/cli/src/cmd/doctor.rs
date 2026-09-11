//! `cambi doctor`: can a run work here, and if not what is wrong with it.
//!
//! The same probes `status` renders, with an opinion attached to each: `ok`,
//! `warn` or `fail`, and a diagnostic with a code and a `help:` line for
//! anything that is not `ok`. Exit 0 if a run can work here — every check `ok`
//! or `warn` — and 1 if any check failed.
//!
//! It runs the two probes `status` will not: `bazel --version`, and a publish
//! plus fetch of a probe object. Those are the two things a user cannot find
//! out by reading their own files, and they are the two that actually break.

use anyhow::Result;
use cambium_engine::{probe, Diagnostic, Level};
use serde::Serialize;

use crate::cmd::{result_event, Context};

/// What `doctor` thinks of one check.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Grade {
    /// Nothing to do.
    Ok,
    /// A run works, and something here will surprise someone eventually.
    Warn,
    /// A run cannot work until this is fixed.
    Fail,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub level: Grade,
    /// What the check found, in a few words. Always present, `ok` included —
    /// "bazel 7.4.1" is the useful half of a passing Bazel check.
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: Grade::Ok,
            detail: detail.into(),
            diagnostic: None,
        }
    }

    /// A check graded by the diagnostic its probe raised: an error diagnostic
    /// is a failure, a warning is a warning. One rule, so that a code's level
    /// and `doctor`'s verdict can never disagree.
    fn from(name: &'static str, detail: impl Into<String>, d: Diagnostic) -> Self {
        let level = match d.level {
            Level::Error => Grade::Fail,
            Level::Warning => Grade::Warn,
        };
        Self {
            name,
            level,
            detail: detail.into(),
            diagnostic: Some(d),
        }
    }
}

pub fn run(ctx: &Context) -> Result<u8> {
    let mut opts = ctx.probe_options();
    opts.run_bazel = true;
    opts.store_round_trip = true;
    let probes = probe::collect(&opts);
    let checks = judge(&probes);
    let worst = checks.iter().map(|c| c.level).max_by_key(level_rank);

    if let Some(fmt) = ctx.format.data("cambi doctor")? {
        match fmt {
            crate::DataFormat::Text => print_human(&checks),
            crate::DataFormat::Json | crate::DataFormat::Jsonl => {
                let doc = serde_json::json!({
                    "schema": cambium_engine::report::SCHEMA_VERSION,
                    "checks": checks,
                });
                if fmt == crate::DataFormat::Jsonl {
                    println!("{}", result_event(&doc)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&doc)?);
                }
            }
        }
    }
    Ok(if worst == Some(Grade::Fail) {
        crate::EXIT_ERROR
    } else {
        crate::EXIT_OK
    })
}

fn level_rank(l: &Grade) -> u8 {
    match l {
        Grade::Ok => 0,
        Grade::Warn => 1,
        Grade::Fail => 2,
    }
}

/// Every probe, graded. The order is the order someone fixes things in: you
/// cannot configure a store for a workspace that is not there.
pub fn judge(p: &probe::Probes) -> Vec<Check> {
    let mut checks = Vec::new();

    // ---- workspace ----
    checks.push(match (&p.workspace.root, p.workspace.mode) {
        (None, _) => Check::from(
            "workspace",
            "none found",
            Diagnostic::error(
                cambium_engine::Code::E004Bazel,
                "no Bazel workspace above the current directory",
            )
            .with_help("run from inside the workspace, or pass -C <dir>"),
        ),
        (Some(root), Some(probe::WorkspaceMode::Ambiguous)) => Check::from(
            "workspace",
            format!("{} (mode undecided)", root.display()),
            p.workspace
                .diagnostics
                .first()
                .cloned()
                .expect("an ambiguous mode always carries its diagnostic"),
        ),
        (Some(root), mode) => Check::ok(
            "workspace",
            format!(
                "{} ({})",
                root.display(),
                match mode {
                    Some(probe::WorkspaceMode::Bzlmod) if p.workspace.inert_workspace =>
                        "bzlmod; the WORKSPACE file is inert".to_string(),
                    Some(probe::WorkspaceMode::Bzlmod) => "bzlmod".to_string(),
                    Some(probe::WorkspaceMode::Workspace) if p.workspace.inert_module_bazel =>
                        "WORKSPACE; the MODULE.bazel file is inert".to_string(),
                    Some(probe::WorkspaceMode::Workspace) => "WORKSPACE".to_string(),
                    _ => "no marker file".to_string(),
                }
            ),
        ),
    });

    // ---- bazel ----
    if let Some(b) = &p.bazel {
        checks.push(match (b.diagnostics.first(), &b.version) {
            (Some(d), _) => Check::from("bazel", b.binary.clone(), d.clone()),
            (None, Some(v)) => Check::ok("bazel", v.clone()),
            (None, None) => Check::ok("bazel", b.binary.clone()),
        });
    }

    // ---- config ----
    let config_detail = format!(
        "{}{}",
        p.config.path.display(),
        if p.config.exists {
            ""
        } else {
            " (absent; the defaults apply)"
        }
    );
    checks.push(match p.config.diagnostics.first() {
        Some(d) => Check::from("config", config_detail, d.clone()),
        None => Check::ok("config", config_detail),
    });

    // ---- store ----
    checks.push(match (p.store.configured, p.store.reachable) {
        (false, _) => Check::ok(
            "store",
            "none configured; every run builds its own index (not an error)",
        ),
        (true, Some(true)) => Check::ok(
            "store",
            match p.store.indexes {
                Some(n) => format!("{}, {n} indexes", p.store.kind),
                None => format!("{}, reachable", p.store.kind),
            },
        ),
        (true, _) => Check::from(
            "store",
            format!("{} unreachable", p.store.kind),
            p.store
                .diagnostics
                .first()
                .cloned()
                .unwrap_or_else(unreachable_fallback),
        ),
    });

    // ---- store round trip ----
    if let Some(r) = &p.round_trip {
        checks.push(match r.diagnostics.first() {
            Some(d) => Check::from("store round trip", "publish and fetch back", d.clone()),
            None => Check::ok("store round trip", format!("wrote and read {}", r.name)),
        });
    }

    // ---- position ----
    if let Some(pos) = &p.position {
        let detail = match (&pos.head, pos.plan, pos.distance) {
            (None, ..) => "HEAD could not be resolved".to_string(),
            (Some(h), probe::Plan::Warm, _) => format!("{} has an index", probe::short(h)),
            (Some(h), probe::Plan::Splice, Some(n)) => {
                format!("{} splices from {n} commits back", probe::short(h))
            }
            (Some(h), _, _) => format!("{} would be a cold build", probe::short(h)),
        };
        // A cold position is not a fault: it is what a first run looks like.
        // Only what the walk *complained* about is graded.
        checks.push(match pos.diagnostics.first() {
            Some(d) => Check::from("position", detail, d.clone()),
            None => Check::ok("position", detail),
        });
    }

    // ---- index format, and the lineage it belongs to ----
    if let Some(i) = &p.index {
        checks.push(match i.diagnostics.first() {
            Some(d) => Check::from(
                "index",
                format!("format version {}", i.format_version),
                d.clone(),
            ),
            None => Check::ok(
                "index",
                format!(
                    "format version {}, {} packages, {} rule targets",
                    i.format_version, i.packages, i.targets
                ),
            ),
        });
    }
    if let Some(l) = &p.lineage {
        checks.push(match l.diagnostics.first() {
            Some(d) => Check::from(
                "lineage",
                format!("{} conflicting in the store", l.conflicting.len()),
                d.clone(),
            ),
            None => Check::ok(
                "lineage",
                match &l.current {
                    Some(c) => format!("{}, {} others in the store", &c[..12], l.others.len()),
                    None => "none yet; a cold build mints one".to_string(),
                },
            ),
        });
    }

    checks
}

fn unreachable_fallback() -> Diagnostic {
    Diagnostic::warning(
        cambium_engine::Code::W007StoreFetchFailed,
        "the index store could not be reached",
    )
    .with_help("check the path, URL or credentials in [baseline].store")
}

fn print_human(checks: &[Check]) {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in checks {
        println!(
            "{:<6} {:<width$}  {}",
            match c.level {
                Grade::Ok => "ok",
                Grade::Warn => "warn",
                Grade::Fail => "fail",
            },
            c.name,
            c.detail,
            width = width
        );
        if let Some(d) = &c.diagnostic {
            println!("       {}[{}]: {}", d.level, d.code, d.message);
            if let Some(h) = &d.help {
                println!("       = help: {h}");
            }
        }
    }
    let failed = checks.iter().filter(|c| c.level == Grade::Fail).count();
    println!();
    if failed == 0 {
        println!("a run can work here");
    } else {
        println!("{failed} check(s) must be fixed before a run can work here");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_engine::Code;

    fn probes() -> probe::Probes {
        // Everything absent, which is the shape outside a workspace: the
        // grading has to survive it, because that is exactly when someone runs
        // `doctor`.
        probe::collect(&probe::Options::new(
            tempfile::tempdir().unwrap().path().join("nowhere"),
        ))
    }

    #[test]
    fn no_workspace_is_a_failure_with_a_code_and_a_nonzero_exit() {
        let checks = judge(&probes());
        let ws = &checks[0];
        assert_eq!(ws.name, "workspace");
        assert_eq!(ws.level, Grade::Fail);
        assert_eq!(ws.diagnostic.as_ref().unwrap().code, Code::E004Bazel);
        assert!(ws.diagnostic.as_ref().unwrap().help.is_some());
    }

    /// A store nobody configured is not a fault: a developer with no store
    /// builds their own indexes and `doctor` must not tell them they are broken.
    #[test]
    fn an_unconfigured_store_is_ok() {
        let checks = judge(&probes());
        let store = checks
            .iter()
            .find(|c| c.name == "store")
            .expect("a store check");
        assert_eq!(store.level, Grade::Ok);
        assert!(store.diagnostic.is_none());
    }

    #[test]
    fn a_warning_diagnostic_grades_warn_and_an_error_grades_fail() {
        let warn = Check::from(
            "x",
            "d",
            Diagnostic::warning(Code::W017BazelVersionUnsupported, "old"),
        );
        assert_eq!(warn.level, Grade::Warn);
        let fail = Check::from("x", "d", Diagnostic::error(Code::E015BazelNotFound, "gone"));
        assert_eq!(fail.level, Grade::Fail);
    }

    #[test]
    fn every_check_says_something_even_when_it_passes() {
        for c in judge(&probes()) {
            assert!(!c.detail.is_empty(), "{} has no detail", c.name);
        }
    }
}
