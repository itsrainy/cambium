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
//!
//! The grading itself is the engine's (`probe::judge`); this verb runs the
//! probes, prints the checks and picks the exit code.

use anyhow::Result;
use cambium_engine::probe::{self, Check, Grade};

use crate::cmd::{emit, Context};

pub fn run(ctx: &Context) -> Result<u8> {
    let mut opts = ctx.probe_options();
    opts.run_bazel = true;
    opts.store_round_trip = true;
    let probes = probe::collect(&opts);
    let checks = probe::judge(&probes);
    let worst = checks.iter().map(|c| c.level).max();

    let doc = serde_json::json!({
        "schema": cambium_engine::report::SCHEMA_VERSION,
        "checks": checks,
    });
    emit(&doc, ctx.format().data("cambi doctor")?, |_| {
        print_human(&checks)
    })?;
    Ok(if worst == Some(Grade::Fail) {
        crate::EXIT_ERROR
    } else {
        crate::EXIT_OK
    })
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
