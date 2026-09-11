//! `cambi affected --dry-run`: the plan, and nothing else.
//!
//! Three or four lines before a two-minute wait. The engine works the plan out
//! (`cambium_engine::plan`); this prints it and picks the exit code.

use anyhow::Result;
use cambium_engine::plan::{PlanMode, RunPlan};

use crate::cmd::{emit, Context};

pub fn run(
    ctx: &Context,
    engine: &cambium_engine::Engine,
    req: &cambium_engine::AffectedRequest,
) -> Result<u8> {
    let plan = engine.plan(req)?;
    for d in &plan.diagnostics {
        ctx.render.warn(d);
    }
    let doc = serde_json::json!({
        "schema": cambium_engine::report::SCHEMA_VERSION,
        "plan": plan,
    });
    emit(&doc, ctx.format.data("cambi affected --dry-run")?, |_| {
        print_human(&plan)
    })?;
    // The same code the run itself would exit with when it already knows the
    // answer is `//...`; every other plan is a plan and exits 0.
    Ok(if plan.mode == PlanMode::Global {
        crate::EXIT_GLOBAL
    } else {
        crate::EXIT_OK
    })
}

fn print_human(plan: &RunPlan) {
    println!(
        "plan for {}..{}",
        cambium_engine::probe::short(&plan.base),
        cambium_engine::probe::short(&plan.head)
    );
    match &plan.index {
        None => println!("  index  none: cold build"),
        Some(i) => println!(
            "  index  {} (lineage {}, {}{})",
            cambium_engine::probe::short(&i.revision),
            &i.lineage[..6.min(i.lineage.len())],
            match i.age_seconds {
                Some(age) => format!("{} old", crate::cmd::status::duration(age)),
                None => "built at an unrecorded time".into(),
            },
            match i.distance {
                Some(0) | None => String::new(),
                Some(n) => format!(", {n} commit{} back", if n == 1 { "" } else { "s" }),
            }
        ),
    }
    println!(
        "  diff   {} changed file{}, {}",
        plan.changed_files,
        if plan.changed_files == 1 { "" } else { "s" },
        match plan.dirty_packages {
            Some(n) => format!("{n} dirty package{}", if n == 1 { "" } else { "s" }),
            None => "dirty packages unknown until queried".into(),
        }
    );
    match &plan.global_reason {
        Some(r) => println!("  cause  {r}"),
        None if plan.index.is_some() => {
            println!("  cause  no invalidator, externals or bazelrc cause fired")
        }
        None => println!("  cause  unknown until queried: there is no index to classify against"),
    }
    println!("  mode   {}", plan.mode);
}
