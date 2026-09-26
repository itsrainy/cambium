//! Shapes for the synthetic generator, and the named presets.

/// A distribution over small non-negative integers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dist {
    Const(u64),
    /// Uniform over `[lo, hi]`.
    Uniform {
        lo: u64,
        hi: u64,
    },
    /// Log-normal with the given median and shape, clamped to `[min, max]`.
    /// Median rather than mu because that is the number people reason about.
    LogNormal {
        median: f64,
        sigma: f64,
        min: u64,
        max: u64,
    },
}

impl Dist {
    pub fn lognormal(median: f64, sigma: f64, min: u64, max: u64) -> Self {
        Dist::LogNormal {
            median,
            sigma,
            min,
            max,
        }
    }

    pub fn sample<R: rand::Rng>(&self, rng: &mut R) -> u64 {
        match *self {
            Dist::Const(v) => v,
            Dist::Uniform { lo, hi } => {
                if hi <= lo {
                    lo
                } else {
                    rng.gen_range(lo..=hi)
                }
            }
            Dist::LogNormal {
                median,
                sigma,
                min,
                max,
            } => {
                use rand_distr::Distribution;
                let d = rand_distr::LogNormal::new(median.max(1e-9).ln(), sigma)
                    .expect("valid lognormal parameters");
                let v = d.sample(rng);
                // Rounding makes the last-ULP differences of libm's exp()
                // irrelevant, which keeps generation reproducible in practice.
                (v.round() as u64).clamp(min, max)
            }
        }
    }

    pub fn mean_hint(&self) -> f64 {
        match *self {
            Dist::Const(v) => v as f64,
            Dist::Uniform { lo, hi } => (lo + hi) as f64 / 2.0,
            Dist::LogNormal { median, sigma, .. } => median * (sigma * sigma / 2.0).exp(),
        }
    }
}

/// The full shape of a synthetic repository.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphSpec {
    pub seed: u64,
    /// Total rule count (approximate: package sizes are sampled).
    pub targets: u64,
    /// Package count. `0` derives it from `targets / targets_per_package`.
    pub packages: u64,
    pub targets_per_package: Dist,
    pub srcs_per_target: Dist,
    pub deps_per_target: Dist,
    /// DAG depth. Deps always go to strictly lower layers.
    pub layers: u32,
    /// `>1` concentrates targets in upper layers (app-heavy), `<1` in lower.
    pub layer_skew: f64,
    pub cross_package_dep_ratio: f64,
    /// `0` = uniform dep choice, `1` = full preferential attachment.
    pub preferential_attachment: f64,
    /// Explicit "everyone depends on this" targets.
    pub hub_count: u32,
    /// Probability that any given target takes a dep on a hub.
    pub hub_fraction: f64,
    pub test_fraction: f64,
    pub test_suite_fraction: f64,
    pub bzl_files: u32,
    pub bzl_loads_per_package: Dist,
    pub exports_files_ratio: f64,
    pub select_ratio: f64,
    pub alias_ratio: f64,
    pub external_dep_ratio: f64,
    pub path_depth: Dist,
}

impl Default for GraphSpec {
    fn default() -> Self {
        Self {
            seed: 0xC0FFEE,
            targets: 2_000,
            packages: 0,
            // Median 6 with a long tail, as observed in real monorepos.
            targets_per_package: Dist::lognormal(6.0, 1.1, 1, 500),
            srcs_per_target: Dist::lognormal(3.0, 0.8, 0, 200),
            deps_per_target: Dist::lognormal(4.0, 0.9, 0, 300),
            layers: 12,
            layer_skew: 1.0,
            cross_package_dep_ratio: 0.6,
            preferential_attachment: 0.5,
            hub_count: 8,
            hub_fraction: 0.15,
            test_fraction: 0.3,
            test_suite_fraction: 0.05,
            bzl_files: 16,
            bzl_loads_per_package: Dist::Uniform { lo: 1, hi: 3 },
            exports_files_ratio: 0.05,
            select_ratio: 0.1,
            alias_ratio: 0.03,
            external_dep_ratio: 0.05,
            path_depth: Dist::Uniform { lo: 1, hi: 4 },
        }
    }
}

impl GraphSpec {
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// The size presets, smallest first.
    pub const SIZES: &[&str] = &["tiny", "small", "medium", "large", "xl", "xxl"];

    /// The shape suffixes a size may carry, as in `large-hubby`.
    pub const SHAPES: &[&str] = &["hubby", "deep", "flat", "wide-packages"];

    /// Named presets: every [`Self::SIZES`] entry, optionally
    /// suffixed with one of [`Self::SHAPES`]: `large-hubby`, `medium-deep`,
    /// `small-flat`, `xl-wide-packages`.
    pub fn preset(name: &str) -> Option<GraphSpec> {
        let (size, shape) = match name.split_once('-') {
            Some((s, sh)) => (s, Some(sh)),
            None => (name, None),
        };
        let mut spec = GraphSpec::default();
        spec.targets = match size {
            "tiny" => 200,
            "small" => 2_000,
            "medium" => 50_000,
            "large" => 500_000,
            "xl" => 2_000_000,
            "xxl" => 10_000_000,
            _ => return None,
        };
        // Bigger repos are deeper and have more .bzl files.
        spec.layers = match size {
            "tiny" | "small" => 8,
            "medium" => 12,
            "large" => 16,
            _ => 20,
        };
        spec.bzl_files = (spec.targets / 200).clamp(4, 4_000) as u32;
        spec.hub_count = (spec.targets / 5_000).clamp(4, 200) as u32;
        match shape {
            None => {}
            Some("hubby") => {
                spec.preferential_attachment = 0.95;
                spec.hub_fraction = 0.45;
                spec.hub_count = spec.hub_count.max(16);
            }
            Some("deep") => {
                spec.layers *= 4;
                spec.deps_per_target = Dist::lognormal(3.0, 0.7, 1, 100);
            }
            Some("flat") => {
                spec.layers = 3;
                spec.preferential_attachment = 0.2;
            }
            Some("wide-packages") => {
                spec.targets_per_package = Dist::lognormal(60.0, 1.0, 1, 5_000);
                spec.path_depth = Dist::Uniform { lo: 1, hi: 2 };
            }
            Some(_) => return None,
        }
        Some(spec)
    }

    /// Every name [`Self::preset`] accepts: the sizes, then each size with each
    /// shape. `presets()` derives the same list from `SIZES` and `SHAPES`, and
    /// a test holds the two together.
    pub const PRESETS: &[&str] = &[
        "tiny",
        "small",
        "medium",
        "large",
        "xl",
        "xxl",
        "tiny-hubby",
        "tiny-deep",
        "tiny-flat",
        "tiny-wide-packages",
        "small-hubby",
        "small-deep",
        "small-flat",
        "small-wide-packages",
        "medium-hubby",
        "medium-deep",
        "medium-flat",
        "medium-wide-packages",
        "large-hubby",
        "large-deep",
        "large-flat",
        "large-wide-packages",
        "xl-hubby",
        "xl-deep",
        "xl-flat",
        "xl-wide-packages",
        "xxl-hubby",
        "xxl-deep",
        "xxl-flat",
        "xxl-wide-packages",
    ];

    /// [`Self::PRESETS`], derived rather than written out.
    pub fn presets() -> Vec<String> {
        let mut out: Vec<String> = Self::SIZES.iter().map(|s| s.to_string()).collect();
        for size in Self::SIZES {
            for shape in Self::SHAPES {
                out.push(format!("{size}-{shape}"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_preset_resolves_and_nothing_else_does() {
        for p in GraphSpec::presets() {
            assert!(GraphSpec::preset(&p).is_some(), "{p}");
        }
        assert_eq!(
            GraphSpec::presets(),
            GraphSpec::PRESETS,
            "PRESETS is SIZES x SHAPES"
        );
        assert!(GraphSpec::preset("nope").is_none());
        assert!(GraphSpec::preset("medium-nope").is_none());
        assert!(GraphSpec::preset("-hubby").is_none());
    }
}
