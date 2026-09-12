//! What a verb says while it runs, in the one vocabulary every sink draws.

/// A step of a run, as the renderer hears it.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// `cambium <verb>  <repo>  <range>  <note>`, printed once at the top.
    Header {
        verb: String,
        repo: String,
        range: String,
        note: String,
    },
    /// A phase opens. `detail` is what it is doing while it runs, printed in
    /// place of the result.
    PhaseStart {
        name: String,
        detail: Option<String>,
    },
    /// A phase closes, with its result and how long it took.
    PhaseEnd {
        name: String,
        result: String,
        elapsed_ms: u64,
    },
    /// A phase that did not have to run, and why.
    PhaseSkipped {
        name: String,
        reason: String,
    },
    /// The column layout for the rows that follow, and their header line.
    RowHeader(Vec<Column>),
    /// One row, one cell per column.
    Row(Vec<Cell>),
    /// Something worth recording once. The live line is already showing it
    /// with a count that ticks, so only the plain renderer prints it.
    Milestone(String),
    /// What the live line should say it is working on. Live only: a renderer
    /// without a live line drops it.
    Current(String),
    Log(Level, String),
    /// The run is over; the live line is erased.
    Done,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warning => "warning",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// How wide a column is. Exactly one column may be [`Width::Flex`]: it takes
/// whatever is left inside the row cap, which is how a commit subject grows
/// on a wide terminal without flinging the numbers at the right edge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Width {
    Fixed(usize),
    Flex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub title: String,
    pub width: Width,
    pub align: Align,
}

impl Column {
    pub fn fixed(title: &str, width: usize, align: Align) -> Self {
        Self {
            title: title.to_string(),
            width: Width::Fixed(width),
            align,
        }
    }

    pub fn flex(title: &str) -> Self {
        Self {
            title: title.to_string(),
            width: Width::Flex,
            align: Align::Left,
        }
    }
}

/// How a cell is painted. Colour is a terminal's business; the text is the
/// same either way, so a pipe and a terminal read alike.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Dim,
    Bold,
    Cyan,
    Green,
    Yellow,
}

/// One cell of a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    /// Text, aligned the way its column asks.
    Text(String),
    /// Text in a style of its own.
    Styled(String, Style),
    /// A commit, laid out compactly: `sha7 MM-DD HH:MM subject…`, with the
    /// date dimmed on a terminal and the subject truncated to fit.
    Commit {
        sha: String,
        date: String,
        subject: String,
    },
}

impl Cell {
    pub fn text(s: impl Into<String>) -> Self {
        Cell::Text(s.into())
    }

    pub fn styled(s: impl Into<String>, style: Style) -> Self {
        Cell::Styled(s.into(), style)
    }
}
