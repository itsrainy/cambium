//! The simulated runs every renderer consumes.
//!
//! Two commands are simulated: the long `cambi profile` walk, and a single
//! `cambi affected` answer. Both are delivered as the same event grammar,
//! so one renderer draws both and nothing can drift between them.

use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::Duration;

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// The repository the demos pretend to work on.
pub const REPO: &str = "envoy";

/// What the run did to the stored index for one commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Splice {
    /// The commit changed nothing the index models.
    Identical,
    /// The index was spliced, rewriting this many targets.
    Spliced { targets: u32 },
    /// The commit went global, so the index was rebuilt.
    Rebuilt,
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub sha: String,
    pub date: String,
    pub subject: String,
}

/// The measured shape of one step, decided up front so that a renderer can
/// be replayed without drifting from another one.
#[derive(Clone, Copy, Debug)]
pub struct Step {
    pub wall_ms: u64,
    pub files: u32,
    pub affected: u32,
    pub tests: u32,
    pub global: Option<&'static str>,
    /// Extra cost of the cold rebuild a global commit forces.
    pub rebuild_ms: u64,
    pub splice: Splice,
}

/// One row inside a phase. Every command renders rows the same way.
#[derive(Clone, Debug)]
pub struct Row {
    pub sha: String,
    pub date: String,
    pub subject: String,
    pub wall_ms: u64,
    pub affected: u32,
    pub splice: Splice,
    pub global: Option<&'static str>,
}

/// The grammar every command speaks.
#[derive(Clone, Debug)]
pub enum Ev {
    /// `cambium <verb>  <repo>  <range>  <note>`
    Header {
        verb: &'static str,
        range: String,
        note: String,
    },
    /// A phase opens. `detail` is what it is doing while it runs.
    PhaseStart {
        name: &'static str,
        detail: Option<String>,
    },
    /// A phase closes, with its result and how long it took.
    PhaseEnd {
        name: &'static str,
        result: String,
        ms: u64,
    },
    /// A phase that did not have to run.
    PhaseSkipped {
        name: &'static str,
        reason: String,
    },
    /// The column header for the rows that follow.
    RowHeader,
    Row(Row),
    /// What the live line should say it is working on.
    Current(String),
    /// A step worth recording once. Live mode carries it on the live line
    /// instead, so only plain mode prints it.
    Milestone(String),
    Log(String),
    Done,
}

/// The whole profile run, known before a single event is sent.
#[derive(Clone, Debug)]
pub struct Sim {
    pub commits: Vec<Commit>,
    pub steps: Vec<Step>,
    pub cold_ms: u64,
    pub worktree_ms: u64,
    pub targets: u64,
    pub tests_total: u64,
    pub packages: u64,
    pub external_repos: u64,
    pub bytes_on_disk: u64,
    pub load_ms: u64,
}

/// Mostly linear, with three short side branches that merge back.
const PARENTS: [&[usize]; 30] = [
    &[],
    &[0],
    &[1],
    &[2],
    &[3],
    &[4],
    &[5],
    &[5],
    &[7],
    &[6],
    &[9, 8],
    &[10],
    &[11],
    &[12],
    &[13],
    &[12],
    &[14, 15],
    &[16],
    &[17],
    &[18],
    &[19],
    &[20],
    &[19],
    &[22],
    &[23],
    &[21],
    &[25, 24],
    &[26],
    &[27],
    &[28],
];

const SUBJECTS: &[&str] = &[
    "http: drop the per-stream buffer when the decoder is done",
    "tls: prefer the session cache over a fresh handshake",
    "upstream: retry budgets are per cluster, not per host",
    "build: move the fuzz corpus out of the default filegroup",
    "router: a weighted cluster with no weight is an error",
    "stats: histogram buckets are configured, not hard coded",
    "quic: the loss detection timer is armed once",
    "access_log: formatters resolve their operators at parse time",
    "config: reject a listener with no filter chain",
    "test: the integration harness waits on readiness, not sleep",
    "tcp_proxy: idle timeout counts bytes, not events",
    "admin: /stats accepts a filter regex",
    "grpc: the async client reuses one stream per pool",
    "ext_authz: a denied check keeps the request headers",
    "deps: bump the boringssl pin to the September branch",
    "wasm: the vm cache is keyed by the module hash",
];

const MERGE_SUBJECTS: &[&str] = &[
    "Merge pull request #38421 from envoyproxy/tls-session-cache",
    "Merge pull request #38460 from envoyproxy/router-weights",
    "Merge pull request #38512 from envoyproxy/quic-loss-timer",
];

const GLOBAL_REASON: &str = ".bazelrc changed";

fn sha(rng: &mut ChaCha8Rng) -> String {
    (0..10)
        .map(|_| {
            let n: u8 = rng.gen_range(0..16);
            std::char::from_digit(n as u32, 16).unwrap()
        })
        .collect()
}

/// Dates walking forward over about ten days, in the `MM-DD HH:MM` a log
/// would print.
struct Clock {
    day: u32,
    hour: u32,
    minute: u32,
}

impl Clock {
    fn new() -> Self {
        Self {
            day: 2,
            hour: 9,
            minute: 14,
        }
    }
    fn advance(&mut self, rng: &mut ChaCha8Rng) {
        self.minute += rng.gen_range(35..400);
        self.hour += self.minute / 60;
        self.minute %= 60;
        self.day += self.hour / 24;
        self.hour %= 24;
    }
    fn stamp(&self) -> String {
        format!("09-{:02} {:02}:{:02}", self.day, self.hour, self.minute)
    }
}

pub fn build() -> Sim {
    let mut rng = ChaCha8Rng::seed_from_u64(0x_ca3b_1_u64);
    let mut clock = Clock::new();
    let mut commits = Vec::with_capacity(PARENTS.len());
    let mut merges = 0usize;
    for parents in PARENTS.iter() {
        let sha = sha(&mut rng);
        let subject = if parents.len() > 1 {
            let s = MERGE_SUBJECTS[merges % MERGE_SUBJECTS.len()].to_string();
            merges += 1;
            s
        } else {
            SUBJECTS[rng.gen_range(0..SUBJECTS.len())].to_string()
        };
        commits.push(Commit {
            sha,
            date: clock.stamp(),
            subject,
        });
        clock.advance(&mut rng);
    }

    // Two slow steps and one global rebuild, at fixed places so every
    // renderer shows the same surprises.
    let slow = [13usize, 24usize];
    let global_at = 18usize;

    let mut steps = Vec::with_capacity(commits.len());
    for idx in 0..commits.len() {
        let files = rng.gen_range(1..40u32);
        // A few commits touch nothing the graph models.
        let affected = if matches!(idx, 4 | 11 | 27) {
            0
        } else {
            rng.gen_range(1..420u32)
        };
        let tests = (affected as f32 * rng.gen_range(0.15..0.45f32)) as u32;
        let wall_ms = if slow.contains(&idx) {
            rng.gen_range(1_400..1_650u64)
        } else {
            rng.gen_range(60..400u64)
        };
        let global = if idx == global_at {
            Some(GLOBAL_REASON)
        } else {
            None
        };
        let splice = if global.is_some() {
            Splice::Rebuilt
        } else if affected == 0 {
            Splice::Identical
        } else {
            Splice::Spliced {
                targets: affected * rng.gen_range(3..10u32),
            }
        };
        steps.push(Step {
            wall_ms,
            files,
            affected,
            tests,
            global,
            rebuild_ms: if global.is_some() { 1_200 } else { 0 },
            splice,
        });
    }

    Sim {
        commits,
        steps,
        cold_ms: 2_500,
        worktree_ms: 210,
        targets: 68_412,
        tests_total: 9_137,
        packages: 4_206,
        external_repos: 61,
        bytes_on_disk: 214 * 1024 * 1024,
        load_ms: 84,
    }
}

/// Log lines the engine would print, keyed by the step they land after.
fn log_after(idx: usize) -> Option<String> {
    match idx {
        2 => Some("cambium: fetched 3 externals".to_string()),
        7 => Some(
            "warning[CAMBI-W004]: 2 packages were re-queried after a glob widened".to_string(),
        ),
        15 => Some("cambium: index spliced, 118 packages rewritten".to_string()),
        21 => Some(
            "warning[CAMBI-W004]: `//test/integration:all` has no owner in the index".to_string(),
        ),
        _ => None,
    }
}

fn nap(ms: u64, speed: f32) {
    if speed <= 0.0 {
        return;
    }
    let scaled = (ms as f32 * speed) as u64;
    if scaled > 0 {
        thread::sleep(Duration::from_millis(scaled));
    }
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The profile walk, sleeping as the real one would.
pub fn spawn(sim: Sim, speed: f32) -> Receiver<Ev> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        macro_rules! send {
            ($e:expr) => {
                if tx.send($e).is_err() {
                    return;
                }
            };
        }
        let head = sim.commits.last().map(|c| c.sha.clone()).unwrap_or_default();
        let from = sim.commits.first().map(|c| c.sha.clone()).unwrap_or_default();
        send!(Ev::Header {
            verb: "profile",
            range: format!("{}..{}", &from[..7], &head[..7]),
            note: format!("{} first-parent commits", sim.commits.len()),
        });

        send!(Ev::PhaseStart {
            name: "worktree",
            detail: Some("detached at HEAD".to_string()),
        });
        send!(Ev::Current("preparing a detached worktree".to_string()));
        nap(sim.worktree_ms, speed);
        send!(Ev::PhaseEnd {
            name: "worktree",
            result: "ready".to_string(),
            ms: sim.worktree_ms,
        });

        send!(Ev::PhaseStart {
            name: "cold index",
            detail: Some("bazel query //...".to_string()),
        });
        send!(Ev::Current("bazel query //...".to_string()));
        nap(sim.cold_ms, speed);
        send!(Ev::PhaseEnd {
            name: "cold index",
            result: format!("{} targets", group(sim.targets)),
            ms: sim.cold_ms,
        });

        send!(Ev::PhaseStart {
            name: "commits",
            detail: Some(format!("{} steps", sim.commits.len())),
        });
        send!(Ev::RowHeader);
        let mut total = 0u64;
        let mut globals = 0usize;
        for (idx, step) in sim.steps.iter().enumerate() {
            let c = &sim.commits[idx];
            send!(Ev::Current(format!("{}  {}", &c.sha[..10], c.subject)));
            nap(step.wall_ms, speed);
            if let Some(reason) = step.global {
                send!(Ev::Current(format!("rebuilding: {reason}")));
                nap(step.rebuild_ms, speed);
                globals += 1;
            }
            let wall = step.wall_ms + step.rebuild_ms;
            total += wall;
            send!(Ev::Row(Row {
                sha: c.sha.clone(),
                date: c.date.clone(),
                subject: c.subject.clone(),
                wall_ms: wall,
                affected: step.affected,
                splice: step.splice,
                global: step.global,
            }));
            if let Some(line) = log_after(idx) {
                send!(Ev::Log(line));
            }
        }
        send!(Ev::PhaseEnd {
            name: "commits",
            result: format!(
                "{} walked, {} global",
                sim.commits.len(),
                globals
            ),
            ms: total,
        });
        send!(Ev::Done);
    });
    rx
}

/// The answer a single `cambi affected` prints on stdout.
#[derive(Clone, Debug)]
pub struct Answer {
    pub targets: u32,
    pub tests: u32,
    pub max_depth: usize,
    /// Targets at each depth, from 0.
    pub by_depth: Vec<u32>,
    pub labels: Vec<String>,
    pub global: Option<&'static str>,
}

const PACKAGES: &[&str] = &[
    "source/common/http",
    "source/common/router",
    "source/common/tls",
    "source/common/upstream",
    "source/extensions/filters/http/ext_authz",
    "source/extensions/filters/network/tcp_proxy",
    "source/extensions/transport_sockets/tls",
    "source/server",
    "test/common/http",
    "test/integration",
    "test/extensions/filters/http/ext_authz",
];

const RULES: &[&str] = &[
    "config", "lib", "interface", "filter_lib", "config_lib", "codec", "integration_test",
    "unit_test", "well_known_names", "utility_lib",
];

/// A right-skewed spread of targets over `depths` levels: a few direct
/// dependents, a bulge two or three levels out, then a long thin tail.
fn by_depth(depths: usize) -> Vec<u32> {
    let n = depths.max(1);
    let scale = (n as f64 * 0.22).max(0.8);
    let raw: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 + 0.5;
            x.powf(1.6) * (-x / scale).exp()
        })
        .collect();
    let sum: f64 = raw.iter().sum();
    let mut out: Vec<u32> = raw
        .iter()
        .map(|r| (r / sum * 412.0).round() as u32)
        .collect();
    // Keep the headline count honest whatever the rounding did.
    let total: u32 = out.iter().sum();
    if let Some(peak) = out.iter_mut().max_by_key(|v| **v) {
        *peak = (*peak as i64 + 412 - total as i64).max(0) as u32;
    }
    out
}

/// A single affected run. `global` swaps the answer for the short one.
pub fn affected_answer(global: bool, depths: usize) -> Answer {
    let mut rng = ChaCha8Rng::seed_from_u64(0x_a11e_c7);
    let by_depth = by_depth(depths);
    let targets: u32 = by_depth.iter().sum();
    let mut labels = Vec::new();
    while labels.len() < 25 {
        let label = format!(
            "//{}:{}",
            PACKAGES[rng.gen_range(0..PACKAGES.len())],
            RULES[rng.gen_range(0..RULES.len())],
        );
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    labels.sort();
    Answer {
        targets,
        tests: 97,
        max_depth: by_depth.len() - 1,
        by_depth,
        labels,
        global: if global { Some(GLOBAL_REASON) } else { None },
    }
}

/// How the index search goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum IndexFound {
    /// An index already sits on the base commit.
    AtBase,
    /// Nothing at the base, but an ancestor within the search window has one.
    Ancestor,
    /// Nothing within the window, so the run builds a cold index.
    None,
}

/// The base the run is looking for, and how far back it will search.
const BASE: &str = "3f2a9c1";
const ANCESTOR: &str = "dbaed12";
const SEARCH: usize = 50;
/// Commits between the ancestor index and the base.
const BEHIND: usize = 24;

/// The commits a splice from the ancestor index has to replay.
fn splice_rows(rng: &mut ChaCha8Rng) -> Vec<Row> {
    let mut clock = Clock::new();
    clock.day = 9;
    (0..BEHIND)
        .map(|i| {
            let affected = if i == 7 || i == 18 {
                0
            } else {
                rng.gen_range(20..600u32)
            };
            let r = Row {
                sha: sha(rng),
                date: clock.stamp(),
                subject: SUBJECTS[rng.gen_range(0..SUBJECTS.len())].to_string(),
                wall_ms: rng.gen_range(40..120u64),
                affected,
                splice: if affected == 0 {
                    Splice::Identical
                } else {
                    Splice::Spliced {
                        targets: affected * rng.gen_range(2..8u32),
                    }
                },
                global: None,
            };
            clock.advance(rng);
            r
        })
        .collect()
}

/// The three commit range one `cambi affected` answers over.
pub fn spawn_affected(found: IndexFound, global: bool, speed: f32) -> Receiver<Ev> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        macro_rules! send {
            ($e:expr) => {
                if tx.send($e).is_err() {
                    return;
                }
            };
        }
        let mut rng = ChaCha8Rng::seed_from_u64(0x_a11e_c7);

        send!(Ev::Header {
            verb: "affected",
            range: "origin/main..HEAD".to_string(),
            note: "3 commits".to_string(),
        });

        send!(Ev::PhaseStart {
            name: "index",
            detail: Some(format!("looking for {BASE}")),
        });
        send!(Ev::Current(format!("looking for {BASE}")));
        nap(12, speed);

        let index_ms = if found == IndexFound::AtBase {
            12
        } else {
            // Nothing on the base: walk back through the ancestors, which is
            // the part worth narrating because it is the slow one.
            send!(Ev::Milestone(format!(
                "none at {BASE}, searching {SEARCH} ancestors"
            )));
            for n in 1..=SEARCH {
                send!(Ev::Current(format!(
                    "none at {BASE}, searching {SEARCH} ancestors … {n} checked"
                )));
                nap(600 / SEARCH as u64, speed);
            }
            31
        };
        let result = match found {
            IndexFound::AtBase => format!("found at {BASE}  (lineage main, 4 h old)"),
            IndexFound::Ancestor => {
                format!("found at {ANCESTOR}  ({BASE}~{BEHIND}, lineage main, 2 d old)")
            }
            IndexFound::None => format!("none within {SEARCH} ancestors, building cold"),
        };
        send!(Ev::PhaseEnd {
            name: "index",
            result,
            ms: index_ms,
        });

        if found == IndexFound::None {
            send!(Ev::PhaseStart {
                name: "cold index",
                detail: Some("bazel query //...".to_string()),
            });
            send!(Ev::Current("bazel query //...".to_string()));
            nap(2_500, speed);
            send!(Ev::PhaseEnd {
                name: "cold index",
                result: "68,412 targets".to_string(),
                ms: 2_500,
            });
        }

        let skip = if global {
            Some(GLOBAL_REASON.to_string())
        } else {
            match found {
                IndexFound::AtBase => Some("index already at base".to_string()),
                IndexFound::None => Some("cold index is at base".to_string()),
                IndexFound::Ancestor => None,
            }
        };
        match skip {
            Some(reason) => send!(Ev::PhaseSkipped {
                name: "splice",
                reason,
            }),
            None => {
                let rows = splice_rows(&mut rng);
                send!(Ev::PhaseStart {
                    name: "splice",
                    detail: Some(format!("{BEHIND} commits  {ANCESTOR}..{BASE}")),
                });
                send!(Ev::RowHeader);
                let mut rewritten = 0u64;
                for r in &rows {
                    send!(Ev::Current(format!("{}  {}", &r.sha[..10], r.subject)));
                    nap(r.wall_ms, speed);
                    if let Splice::Spliced { targets } = r.splice {
                        rewritten += targets as u64;
                    }
                    send!(Ev::Row(r.clone()));
                }
                send!(Ev::PhaseEnd {
                    name: "splice",
                    result: format!(
                        "{BEHIND} commits, {} rows rewritten, index now at {BASE}",
                        group(rewritten)
                    ),
                    ms: 1_800,
                });
            }
        }

        send!(Ev::PhaseStart {
            name: "query",
            detail: Some("12 changed files".to_string()),
        });
        send!(Ev::Current("walking reverse dependencies".to_string()));
        nap(40, speed);
        send!(Ev::PhaseEnd {
            name: "query",
            result: if global {
                "the whole repository".to_string()
            } else {
                "12 changed files, 4 packages".to_string()
            },
            ms: 40,
        });
        send!(Ev::Done);
    });
    rx
}
