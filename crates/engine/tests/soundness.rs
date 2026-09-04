//! Randomised soundness sweep (spec 12.4).
//!
//! Many seeds, many revisions, all mutation kinds: after each one the engine's
//! answer must contain everything the naive reference algorithm says is
//! affected. Under-approximation is the one bug class the design does not
//! tolerate, so this is the test that matters most.
//!
//! The `small` sweep runs on every change. Set `CAMBIUM_SOUNDNESS_SIZE=medium`
//! (or a preset name) and `CAMBIUM_SOUNDNESS_SEEDS=N` for the nightly run.

use std::collections::BTreeSet;

use cambium_engine::{AffectedRequest, Config, Engine, SynthQuerier};
use cambium_synth::{commit, generate, reference::reference_affected, ChangeSpec, GraphSpec};

fn engine() -> Engine {
    let dir = std::env::temp_dir().join("cambium-tests");
    let _ = std::fs::create_dir_all(&dir);
    Engine::with_parts(
        Config::default(),
        cambium_diff::Repo::new(dir.clone()),
        cambium_bazel::Bazel::at(dir, Some("/nonexistent/bazel")),
    )
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[test]
fn engine_never_under_approximates() {
    let preset = env_or("CAMBIUM_SOUNDNESS_SIZE", "small");
    let seeds: u64 = env_or("CAMBIUM_SOUNDNESS_SEEDS", "12").parse().unwrap();
    let revisions: u64 = env_or("CAMBIUM_SOUNDNESS_REVISIONS", "8").parse().unwrap();
    let e = engine();
    let req = AffectedRequest {
        base: "base".into(),
        head: "head".into(),
        include_manual: true,
        explain: false,
        no_full_build: true,
    };

    let mut exact = 0usize;
    let mut over = 0usize;
    for seed in 0..seeds {
        let mut g = generate(&GraphSpec::preset(&preset).unwrap().with_seed(seed));
        let mut index = e
            .full_build_with("c0", &SynthQuerier::new(&g))
            .expect("full build");
        // Cycle the change presets so every mutation kind is exercised.
        for rev in 0..revisions {
            let preset_name =
                ChangeSpec::PRESETS[(rev as usize + seed as usize) % ChangeSpec::PRESETS.len()];
            let spec = ChangeSpec {
                seed: seed * 1000 + rev,
                ..ChangeSpec::preset(preset_name).unwrap()
            };
            let before = g.clone();
            let c = commit(&mut g, &spec, rev);
            if c.changes.is_empty() {
                continue;
            }
            let out = e
                .compute(index, &c.changes, &SynthQuerier::new(&g), &req)
                .unwrap_or_else(|e| panic!("seed {seed} rev {rev} ({preset_name}): {e}"));
            let ours: BTreeSet<String> = out.report.affected.iter().cloned().collect();
            let theirs = reference_affected(&before, &g);
            let missing: Vec<&String> = theirs.difference(&ours).collect();
            assert!(
                missing.is_empty(),
                "seed {seed} rev {rev} ({preset_name}) missed {} targets: {:?}\nmutations: {:?}",
                missing.len(),
                missing.iter().take(5).collect::<Vec<_>>(),
                c.mutations,
            );
            if ours == theirs {
                exact += 1;
            } else {
                over += 1;
            }
            match out.index {
                Some(i) => index = i,
                None => {
                    // Global invalidation: rebuild and carry on.
                    index = e.full_build_with("c", &SynthQuerier::new(&g)).unwrap();
                }
            }
        }
    }
    eprintln!("soundness sweep: {exact} exact, {over} over-approximating");
}
