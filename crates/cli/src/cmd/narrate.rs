//! The pieces of the stderr narration more than one verb draws.
//!
//! Every long-running verb opens with a header naming the repository;
//! `affected`, `profile` and `index build` all announce an index built from
//! scratch; and `affected` and `profile` both lay a commit out as a row. Each
//! piece lives here once, so the verbs read alike and a new [`IndexWork`]
//! variant is drawn the same way in both.

use cambium_engine::profile::IndexWork;
use cambium_ui::{Align, Cell, Column, Event, Sink, Style};

use crate::cmd::short;

/// The five columns of a commit row, live and in `profile`'s final table
/// alike, so the two cannot drift. `first` names the flexible column: `commit`
/// for a profile step, `range` for the splice `affected` replays.
pub(crate) fn commit_columns(first: &str) -> Vec<Column> {
    vec![
        Column::flex(first),
        Column::fixed("time", 9, Align::Right),
        Column::fixed("affected", 8, Align::Right),
        Column::fixed("tests", 6, Align::Right),
        Column::fixed("index", 17, Align::Left),
    ]
}

/// The `index` cell: what the run did to the stored index for this commit.
/// Dim when nothing happened, so it adds no noise.
pub(crate) fn index_cell(work: &IndexWork) -> Cell {
    match work {
        IndexWork::Identical => Cell::styled("= unchanged", Style::Dim),
        IndexWork::Spliced { rows_rewritten } => {
            Cell::styled(format!("~ {rows_rewritten:>5} rewritten"), Style::Cyan)
        }
        IndexWork::Rebuilt { .. } => Cell::styled("\u{21bb} rebuilt", Style::Cyan),
        IndexWork::Failed => Cell::styled("x failed", Style::Yellow),
    }
}

/// The workspace directory's name, which the narration header calls the repo.
pub(crate) fn repo_name(engine: &cambium_engine::Engine) -> String {
    engine
        .workspace()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Opens the phase that indexes `commit` from scratch: the query it runs, and
/// the live line while it runs. `phase` is `cold index` inside a run that
/// needed one, and `index` for `cambi index build`, whose whole job it is.
pub(crate) fn start_indexing(sink: &mut dyn Sink, phase: &str, commit: &str) {
    sink.emit(Event::PhaseStart {
        name: phase.into(),
        detail: Some(format!("bazel query //... at {}", short(commit))),
    });
    sink.emit(Event::Current(format!("indexing {}", short(commit))));
}
