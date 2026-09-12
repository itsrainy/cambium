//! The terminal output grammar every `cambi` verb renders through.
//!
//! One renderer draws every command, so nothing can drift between them. The
//! grammar is four things and no more:
//!
//! ```text
//! cambium profile  envoy  e6b1317..cc1b5cb  30 first-parent commits
//!   worktree .............  ready                                     210 ms
//!   cold index ...........  68,412 targets                             2.5 s
//!     commit               time affected  index
//!     e6b1317 09-02 09:14 tcp_proxy: idle t…  116 ms      375  ~  2625 rewritten
//!   commits ..............  30 walked, 1 global                        9.9 s
//! ```
//!
//! 1. a header line, `cambium <verb>  <repo>  <range>  <note>`;
//! 2. phase lines, `  <name> <dots> <result>  <time>`;
//! 3. rows inside a phase, in fixed columns, never wider than 96;
//! 4. one live bottom line, erased before anything else is printed.
//!
//! A caller builds [`Event`]s and hands them to a [`Sink`]. Which sink it
//! gets is [`OutputMode`], decided once at startup. [`Stream`] has the live
//! line and rewrites a finished phase in place; [`Plain`] prints the same
//! bytes without it; [`Jsonl`] prints one JSON object per event; [`Null`]
//! prints nothing. Rows are byte for byte identical between `Stream` and
//! `Plain`, which the tests pin.

pub mod hist;
pub mod report;

mod event;
mod mode;
mod paint;
pub mod row;
mod sink;

pub use event::{Align, Cell, Column, Event, Level, Style, Width};
pub use mode::{ColorChoice, Kind, OutputMode};
pub use paint::Paint;
pub use row::{terminal_width, ROW_CAP};
pub use sink::{boxed, Jsonl, Null, Plain, Sink, Stream};
