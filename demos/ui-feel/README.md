# ui-feel

The terminal grammar for cambium's commands, built as a runnable demo so
the look and feel can be judged before it is implemented.

This is a standalone crate. It is deliberately not a workspace member (the
root `Cargo.toml` has `exclude = ["demos"]`), so a workspace-wide build
never pulls in ratatui, rasciigraph or textplots.

```
cd demos/ui-feel
cargo build
./target/debug/ui-feel <subcommand>
```

## The grammar

Every command renders the same four things, and one renderer
(`src/stream.rs`) draws all of them. It takes a single `live: bool`: with it
the run has a bottom line and finished phases are rewritten in place;
without it the same output is printed straight down a pipe.

```
cambium profile  envoy  e6b1317..cc1b5cb  30 first-parent commits
  worktree .............  ready                                     210 ms
  cold index ...........  68,412 targets                             2.5 s
  commits .............. 30 steps
    commit     date        subject                  time affected  index
    e6b1317b77 09-02 09:14 tcp_proxy: idle timeo…  116 ms      375  ~  2625 rewritten
    ...
  commits ..............  30 walked, 1 global                        9.9 s
```

1. **A header line**: `cambium <verb>  <repo>  <range>  <note>`.
2. **Phase lines**: `  <name> <dots to a fixed column> <result>  <time>`.
   The line is printed when the phase starts, with its detail in place of
   the result. In live mode the finished line is written back over it; in
   plain mode, and in live mode once the phase has printed rows, the
   finished line is printed underneath instead. A phase that did not have to
   run says `skipped (<reason>)`.
3. **Rows** inside a phase, in fixed columns: the commit, then `time
   affected  index`. A row never grows past 96 columns, so on a wide
   terminal the numbers stay beside the text instead of being flung at the
   right edge; the commit cell takes whatever is left inside that cap, and
   the subject is truncated with `…` to fit it. Width is measured at each
   print, falling back to 80 columns when there is no terminal to measure.
4. **One live bottom line**, `⠹ <what it is doing>   <m:ss>`, erased before
   anything else is printed, so it never interleaves with a row.

Rows are byte for byte identical between `stream` and `plain`; the only
difference in the transcript is the phase-start lines that `stream`
overwrites.

## The `index` column

What the run did to the stored index for each commit: `= unchanged` when the
commit changed nothing the graph models (dimmed, so it adds no noise),
`~   264 rewritten` with the count of targets rewritten (right aligned, so
the column lines up), and `↻ rebuilt` when the commit went global.

## Subcommands

| subcommand | what it shows |
|------------|---------------|
| `stream`   | `cambi profile` at a terminal: the whole grammar with the live line and in-place phase results. |
| `plain`    | The same run with the live line omitted and every phase printing both its lines, so a pipe or a CI log is never silent. |
| `affected` | `cambi affected origin/main..HEAD`: phases `index`, `splice` and `query` on stderr, then the answer on stdout: the target and test counts, the first 25 labels with `… 387 more`, and last the targets-by-depth distribution. `--all` drops the elision; `--global` answers the way a commit that invalidates the whole graph does, with `splice` skipped and no list. |
| `chart`    | Ends of runs only: the per-commit wall times drawn twice, once by `rasciigraph` as a line plot in walk order and once by `textplots` as a braille histogram, so the two crates can be compared. Shape and range only, one caption line each. |
| `full`     | The other case, `cambium-bench watch`: a fullscreen ratatui dashboard on the alternate screen, three subjects by three tools, a table with step, progress and ETA, a braille `Chart` of wall time per tool, and a `BarChart` of totals. `q` quits. |
| `all`      | Every demo above, in order. |

## Flags

- `--speed <f32>` (global) multiplies every simulated sleep. `--speed 0`
  runs the whole stream instantly, which is what the smoke tests use;
  `--speed 0.3` is a good pace for flipping between demos.
- `--commit compact|columns` (global, default `compact`) lays a commit out
  either as one left-aligned cell, `e6b1317 09-02 09:14 tcp_proxy: idle …`
  with the date dimmed, or as three columns with their own headers. It
  applies to the live rows and to the final table.
- `--index at-base|ancestor|none` (global, default `ancestor`) decides what
  the `index` phase finds, which is the part of a real run most worth
  narrating because it is where the time goes:
  - `at-base`: an index already sits on the base, so it is `found at
    3f2a9c1  (lineage main, 4 h old)` in 12 ms and `splice` says
    `skipped (index already at base)`.
  - `ancestor`: nothing at the base, so the run walks back through 50
    ancestors (the live line counts them off, `… 37 checked`) and finds
    `dbaed12  (3f2a9c1~24, lineage main, 2 d old)`. `splice` then replays
    the 24 commits between them as rows and ends with how many rows it
    rewrote and where the index now sits.
  - `none`: the search finds nothing, so the run says `none within 50
    ancestors, building cold`, a `cold index` phase runs `bazel query
    //...`, and `splice` says `skipped (cold index is at base)`.

  In live mode the search count only ever appears on the live line. In plain
  mode the milestone `none at 3f2a9c1, searching 50 ancestors` is printed
  once as a line, and the per-ancestor ticking is not printed at all.
- `--hist columns|bars` (global, default `columns`) draws `affected`'s
  targets-by-depth distribution either as a vertical column chart, eight
  rows tall, with each value printed directly above its own column and the
  depth labels centred under an axis line, or as horizontal block bars with
  the count after each one. The column chart is hand rolled: columns are
  `█` with a fractional top drawn from `▁▂▃▄▅▆▇`, so a bin with one target
  in it is still visible. It never exceeds the 96 column cap: past that it
  drops to every other value, then every other depth label, and only then
  narrows the bars.
- `--depths N` (global, default 7) spreads the affected targets over N
  levels, so the chart can be seen at 7, 12, 25 and more. A depth inside the
  range with no targets still gets its column and its label.
- `--report prose|prose-last|kv` (global, default `kv`) picks the final
  report `stream` and `plain` print. `prose` is the two paragraphs then the
  table, as `print_human` prints today. `prose-last` puts the table first
  and then the paragraphs with every number in bold (on a TTY only). `kv`
  replaces the paragraphs with a compact aligned key/value block.

Per docs/CLI.md the answer goes to stdout and all progress to stderr, so
`ui-feel affected > targets.txt` still shows the phases running.

## Running them

```
./target/debug/ui-feel stream
./target/debug/ui-feel plain
./target/debug/ui-feel affected
./target/debug/ui-feel affected --global
./target/debug/ui-feel --hist bars affected
./target/debug/ui-feel --depths 25 affected
./target/debug/ui-feel --index none affected
./target/debug/ui-feel --index at-base affected
./target/debug/ui-feel --commit columns stream
./target/debug/ui-feel chart
./target/debug/ui-feel full

./target/debug/ui-feel --report prose-last stream
./demo.sh            # all of them in order, pausing between
```

`full` needs a real terminal; outside one it says so and falls back to the
plain renderer, so every subcommand runs to completion under `--speed 0` in
a pipe.

## Notes on the crates

The column chart, the bar chart and the target list are plain text with no
ANSI in them, so `ui-feel affected | less` and a CI log see exactly what a
terminal does.

- `rasciigraph` 0.3 always prints a value gutter and an axis column. "Shape
  and range only" therefore means cutting everything left of the plotting
  area after the fact, which `strip_axis` in `src/chart.rs` does.
- `textplots` 0.8 draws axes and labels in its `Display` impl, not in the
  canvas. Calling `figures()` and then `frame()` gives the bare braille
  canvas, and avoids the crate's awkward `&'a mut Chart<'a>` builder chain.
- `indicatif` and `renderdag` were tried in an earlier round and dropped
  with the renderers that used them. ratatui and crossterm stay only because
  `full` is a fullscreen dashboard; `stream` needs neither.
