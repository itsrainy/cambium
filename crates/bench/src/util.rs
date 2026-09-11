//! The small things every module needs, spelled once.
//!
//! Nothing here is benchmark logic: a percentile, the clock, the cache
//! directory a fetch script installs into, the filename a Bazel version is
//! stamped into. Each has exactly one definition so that two result files
//! cannot disagree about how a version is escaped or a timestamp is written.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;

/// The `q`th percentile of an already-sorted sample, by nearest rank. Empty is
/// 0: a distribution over no runs has no shape, and the reports say `n/a`
/// beside it rather than reading the number.
pub fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
}

/// Seconds since the epoch, or 0 if the clock is before it. A result file's
/// timestamp is for a reader, so a broken clock must not fail a run.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Now, as RFC 3339 in UTC: `2026-09-10T14:02:11Z`.
///
/// Every timestamp the harness writes for a *reader* is this shape — the
/// progress file, the per-step stamps in the state file — because a watcher
/// subtracts them and a person reads them, and `now_unix` above serves
/// neither. No `chrono`: the conversion is twenty lines of civil-calendar
/// arithmetic and a dependency that parses every timezone on earth is not
/// worth carrying for one format.
pub fn rfc3339_now() -> String {
    rfc3339(now_unix())
}

/// `secs` since the epoch as RFC 3339 in UTC.
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// An RFC 3339 timestamp back to seconds since the epoch, or `None` when it is
/// not one. Only the shape [`rfc3339`] writes is accepted, plus an offset of
/// `Z` or `+00:00`: the harness is the only writer, and guessing at a local
/// time would make an elapsed reading silently hours wrong.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let time = rest
        .strip_suffix('Z')
        .or_else(|| rest.strip_suffix("+00:00"))
        .or_else(|| rest.strip_suffix("+0000"))?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut tp = time.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let min: u64 = tp.next()?.parse().ok()?;
    // Fractional seconds are accepted and dropped: a progress file is polled
    // once a second.
    let sec: u64 = tp.next()?.split('.').next()?.parse().ok()?;
    if tp.next().is_some() || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + h * 3600 + min * 60 + sec)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to `(y, m, d)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// Its inverse, so a timestamp the harness wrote can be read back.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// How long `secs` is, for a table: `12s`, `4m12s`, `2h34m`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// The short git sha of the cambium that was built, or `"unknown"`.
///
/// Stamped in at build time by `build.rs`, and *not* asked of `git` at run
/// time: a workload's working directory is a **subject's** checkout, so
/// `git rev-parse HEAD` there answers with envoy's HEAD, not ours.
pub fn tool_sha() -> &'static str {
    match option_env!("CAMBIUM_GIT_SHA") {
        Some(s) if !s.is_empty() => s,
        _ => "unknown",
    }
}

/// `$HOME`, or the system temp directory when there is none — a CI runner or a
/// daemon with no home still has to put a cache somewhere.
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `~/.cache/cambium/<name>`: where the `tools/*/fetch.sh` scripts install and
/// where the subjects, mirrors and stores live. Every caller lets an
/// environment variable win first; this is only the fallback.
pub fn cache_dir(name: &str) -> PathBuf {
    home().join(".cache/cambium").join(name)
}

/// `<stem>[-<version>]`, with the characters a Bazel version may carry but a
/// path component may not (`/`, a space, `:`) replaced by `_`.
///
/// Results and output bases are keyed by the Bazel that produced them, because
/// the same subject at the same pin answers differently under 7 and under 9.
/// An empty version is no version: `--bazel-version ''` means "whatever the
/// worktree asks for".
pub fn versioned_stem(stem: &str, version: Option<&str>) -> String {
    match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("{stem}-{}", v.replace(['/', ' ', ':'], "_")),
        None => stem.to_string(),
    }
}

/// [`versioned_stem`] with an extension.
pub fn versioned_name(stem: &str, version: Option<&str>, ext: &str) -> String {
    format!("{}.{ext}", versioned_stem(stem, version))
}

/// A size as a person types one: `1000`, `1e6`, `2M`, `500k`.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1_000.0),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1_000_000.0),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1_000_000_000.0),
        _ => (s, 1.0),
    };
    let v: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("cannot parse size {s:?}"))?;
    Ok((v * mult) as u64)
}

/// Just the `schema` field, so a file can be refused before it is parsed into
/// a type whose fields have since been renamed.
#[derive(serde::Deserialize)]
struct SchemaProbe {
    #[serde(default)]
    schema: u32,
}

/// Reads a result file, refusing one this binary cannot read.
///
/// Every result carries [`crate::RESULT_SCHEMA`], and the whole point of the
/// field is that an older file is *not* silently accepted: serde's `default`
/// attributes would happily fill a renamed field with a zero and the report
/// would print it. The three readers each did a bare `from_str` instead, so
/// the version was written and never checked.
pub fn read_result<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let probe: SchemaProbe = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a result file", path.display()))?;
    anyhow::ensure!(
        probe.schema == crate::RESULT_SCHEMA,
        "{} is schema {} and this cambium reads {}; delete it and run the workload again",
        path.display(),
        probe.schema,
        crate::RESULT_SCHEMA
    );
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentile_is_the_nearest_rank_and_an_empty_sample_is_zero() {
        let s = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&s, 0.5), 60);
        assert_eq!(percentile(&s, 0.9), 100);
        assert_eq!(percentile(&s, 1.0), 100, "q=1 does not run off the end");
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.9), 7);
    }

    #[test]
    fn a_version_that_cannot_be_a_filename_is_escaped() {
        assert_eq!(versioned_name("index-abc", None, "bin"), "index-abc.bin");
        assert_eq!(
            versioned_name("index-abc", Some(""), "bin"),
            "index-abc.bin"
        );
        assert_eq!(
            versioned_name("history-abc", Some("7.x"), "json"),
            "history-abc-7.x.json"
        );
        assert_eq!(
            versioned_name("index-abc", Some("last/green"), "bin"),
            "index-abc-last_green.bin"
        );
        assert_eq!(
            versioned_name("changes-abc", Some("bazel 7.4.1"), "json"),
            "changes-abc-bazel_7.4.1.json"
        );
    }

    #[test]
    fn a_result_from_another_schema_is_refused_rather_than_half_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.json");

        std::fs::write(&path, r#"{"schema": 1, "repo": "bazel"}"#).unwrap();
        let e = read_result::<serde_json::Value>(&path)
            .unwrap_err()
            .to_string();
        assert!(e.contains("schema 1"), "{e}");

        // A file with no `schema` at all is from before the field existed,
        // which is just as unreadable.
        std::fs::write(&path, r#"{"repo": "bazel"}"#).unwrap();
        assert!(read_result::<serde_json::Value>(&path).is_err());

        std::fs::write(
            &path,
            format!(r#"{{"schema": {}, "repo": "bazel"}}"#, crate::RESULT_SCHEMA),
        )
        .unwrap();
        let v: serde_json::Value = read_result(&path).unwrap();
        assert_eq!(v["repo"], "bazel");
    }

    #[test]
    fn a_timestamp_round_trips_through_rfc_3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_789_048_931), "2026-09-10T14:02:11Z");
        for secs in [
            0,
            1,
            86_399,
            86_400,
            951_782_400,
            1_789_048_931,
            4_102_444_800,
        ] {
            assert_eq!(parse_rfc3339(&rfc3339(secs)), Some(secs), "{secs}");
        }
        // A leap day, which is where a hand-written calendar goes wrong.
        assert_eq!(rfc3339(1_583_020_800), "2020-03-01T00:00:00Z");
        assert_eq!(parse_rfc3339("2020-02-29T23:59:59Z"), Some(1_583_020_799));
        assert_eq!(
            parse_rfc3339("2026-09-10T14:02:11+00:00"),
            Some(1_789_048_931)
        );
        assert_eq!(
            parse_rfc3339("2026-09-10T14:02:11.443Z"),
            Some(1_789_048_931)
        );
    }

    #[test]
    fn anything_that_is_not_a_utc_timestamp_is_refused() {
        // A local time would make an elapsed reading hours wrong in silence.
        for bad in [
            "",
            "2026-09-10",
            "2026-09-10T14:02:11+02:00",
            "2026-09-10T14:02:11",
            "2026-13-10T00:00:00Z",
            "2026-09-10T24:00:00Z",
            "not a time",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_size_takes_a_suffix_or_an_exponent() {
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert_eq!(parse_size("1e6").unwrap(), 1_000_000);
        assert_eq!(parse_size("2M").unwrap(), 2_000_000);
        assert_eq!(parse_size(" 500k ").unwrap(), 500_000);
        assert_eq!(parse_size("1G").unwrap(), 1_000_000_000);
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn a_duration_is_readable_at_every_scale() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(252), "4m12s");
        assert_eq!(duration(9_240), "2h34m");
    }
}
