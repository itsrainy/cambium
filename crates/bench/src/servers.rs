//! Stopping the Bazel servers a subject's workload left running.
//!
//! Every workspace directory a workload runs Bazel in has a server of its own:
//! the subject's checkout (under the configured output base, and again under
//! the one Bazel derives when a tool drops that flag), the base worktree
//! bazel-diff reads, Tango's base and head worktrees, and the head worktree
//! target-determinator is handed. Bazel keeps an idle
//! server for three hours and each holds a gigabyte or more, so a run over
//! several subjects that leaves them all up runs a 32 GB machine out of memory
//! before its last subject. A workload stops its subject's servers when it
//! ends, including when it fails.
//!
//! The servers are found from their output bases rather than by running
//! `bazel shutdown` in each workspace, because Tango deletes its worktrees when
//! it is done and a workspace that is gone cannot be shut down from. Every
//! output base records its workspace in `DO_NOT_BUILD_HERE` and its server's
//! pid in `server/server.pid.txt`; a live server whose workspace is inside one
//! of the subject's directories is sent SIGTERM, which Bazel treats as a
//! shutdown.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where Bazel puts the output bases it derives: `--output_user_root`'s
/// default for this user.
pub fn output_user_root() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_default();
    if cfg!(target_os = "macos") {
        PathBuf::from(format!("/private/var/tmp/_bazel_{user}"))
    } else {
        crate::util::home()
            .join(".cache/bazel")
            .join(format!("_bazel_{user}"))
    }
}

/// A server an output base says it has.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Server {
    pub output_base: PathBuf,
    pub workspace: PathBuf,
    pub pid: u32,
}

/// The servers recorded in the output bases under `search` whose workspace is
/// inside one of `roots`. Recorded, not necessarily running: a pid file
/// outlives its server.
pub fn find(search: &[PathBuf], roots: &[PathBuf]) -> Vec<Server> {
    let roots = with_canonical(roots);
    let mut out = Vec::new();
    for dir in search {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in rd.flatten() {
            let output_base = e.path();
            let Ok(ws) = std::fs::read_to_string(output_base.join("DO_NOT_BUILD_HERE")) else {
                continue;
            };
            let workspace = PathBuf::from(ws.trim());
            if !roots.iter().any(|r| workspace.starts_with(r)) {
                continue;
            }
            let Some(pid) = std::fs::read_to_string(output_base.join("server/server.pid.txt"))
                .ok()
                .and_then(|p| p.trim().parse().ok())
            else {
                continue;
            };
            out.push(Server {
                output_base,
                workspace,
                pid,
            });
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Each root, and its canonical spelling when that differs: Bazel records the
/// canonical workspace (`/private/tmp/...` for `/tmp/...` on macOS).
fn with_canonical(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for r in roots {
        out.push(r.clone());
        if let Ok(c) = r.canonicalize() {
            if c != *r {
                out.push(c);
            }
        }
    }
    out
}

/// Whether `s.pid` is still the server of `s.output_base`: the process's
/// arguments name that output base. A pid that has been reused by something
/// else by now does not.
fn running(s: &Server) -> bool {
    let Ok(out) = Command::new("ps")
        .args(["-p", &s.pid.to_string(), "-o", "args="])
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .contains(&format!("--output_base={}", s.output_base.display()))
}

/// The pid of the server running on `output_base`, if one is: its pid file
/// names a live process whose arguments name that output base.
pub fn running_on(output_base: &Path) -> Option<u32> {
    let pid = std::fs::read_to_string(output_base.join("server/server.pid.txt"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let s = Server {
        output_base: output_base.to_path_buf(),
        workspace: PathBuf::new(),
        pid,
    };
    running(&s).then_some(pid)
}

/// Stops every running server [`find`] reports and waits up to `wait` for
/// them to exit. Answers how many were stopped.
pub fn stop(search: &[PathBuf], roots: &[PathBuf], wait: Duration) -> usize {
    let live: Vec<Server> = find(search, roots).into_iter().filter(running).collect();
    for s in &live {
        tracing::info!(
            pid = s.pid,
            workspace = %s.workspace.display(),
            "stopping a bazel server the workload left running"
        );
        let _ = Command::new("kill")
            .args(["-TERM", &s.pid.to_string()])
            .stderr(Stdio::null())
            .status();
    }
    let deadline = Instant::now() + wait;
    while live.iter().any(running) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    live.len()
}

/// Stops a subject's Bazel servers when dropped, so a workload that fails part
/// way leaves none up either.
pub struct StopOnDrop {
    search: Vec<PathBuf>,
    roots: Vec<PathBuf>,
}

impl StopOnDrop {
    /// For a subject checked out at `worktree`, whose tools work under
    /// `scratch`, run with these Bazel startup flags: the derived output bases
    /// are searched, and so is the directory the configured one is in.
    pub fn subject(worktree: &Path, scratch: &Path, startup_flags: &[String]) -> Self {
        let mut search = vec![output_user_root()];
        for f in startup_flags {
            if let Some(parent) = f
                .strip_prefix("--output_base=")
                .and_then(|ob| Path::new(ob).parent())
            {
                search.push(parent.to_path_buf());
            }
        }
        Self {
            search,
            roots: vec![worktree.to_path_buf(), scratch.to_path_buf()],
        }
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let n = stop(&self.search, &self.roots, Duration::from_secs(30));
        if n > 0 {
            tracing::info!(servers = n, "stopped the subject's bazel servers");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// A stand-in server: a process whose arguments name `output_base`, and
    /// the two files Bazel writes into one.
    fn fake_server(output_base: &Path, workspace: &Path, marked: bool) -> std::process::Child {
        std::fs::create_dir_all(output_base.join("server")).unwrap();
        std::fs::write(
            output_base.join("DO_NOT_BUILD_HERE"),
            workspace.display().to_string(),
        )
        .unwrap();
        let script = match marked {
            true => format!(
                "exec -a 'java --output_base={}' sleep 60",
                output_base.display()
            ),
            false => "exec sleep 60".to_string(),
        };
        let child = Command::new("bash").arg("-c").arg(script).spawn().unwrap();
        std::fs::write(
            output_base.join("server/server.pid.txt"),
            child.id().to_string(),
        )
        .unwrap();
        child
    }

    #[test]
    fn a_subjects_servers_are_stopped_and_nobody_elses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let search = root.join("user_root");
        let subject = root.join("results/envoy");
        let mut ours = fake_server(
            &search.join("a"),
            &subject.join("scratch/tango-worktrees/head"),
            true,
        );
        let mut theirs = fake_server(&search.join("b"), &root.join("results/envoy-2"), true);
        // A pid file whose number now belongs to some other process.
        let mut reused = fake_server(&search.join("c"), &subject, false);
        std::thread::sleep(Duration::from_millis(300));

        let (search, roots) = ([search], [subject]);
        assert_eq!(find(&search, &roots).len(), 2);
        assert_eq!(stop(&search, &roots, Duration::from_secs(10)), 1);
        assert_eq!(ours.wait().unwrap().signal(), Some(15));
        assert!(
            theirs.try_wait().unwrap().is_none(),
            "another subject's server"
        );
        assert!(
            reused.try_wait().unwrap().is_none(),
            "not a server any more"
        );
        let _ = theirs.kill();
        let _ = reused.kill();
        let _ = theirs.wait();
        let _ = reused.wait();
    }

    #[test]
    fn a_server_is_running_on_its_own_output_base_only_while_it_lives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ob = root.join("output/envoy");
        let mut server = fake_server(&ob, &root.join("envoy"), true);
        let mut impostor = fake_server(&root.join("output/xla"), &root.join("xla"), false);
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(running_on(&ob), Some(server.id()));
        // A pid file whose process does not name the output base.
        assert_eq!(running_on(&root.join("output/xla")), None);
        // No pid file at all.
        assert_eq!(running_on(&root.join("output/bazel")), None);

        let _ = server.kill();
        let _ = server.wait();
        assert_eq!(running_on(&ob), None, "a pid file outlives its server");
        let _ = impostor.kill();
        let _ = impostor.wait();
    }
}
