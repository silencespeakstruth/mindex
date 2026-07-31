# Changelog

## Unreleased

Indexing becomes something you can watch, and Research becomes a first-class
surface rather than a panel you have to know about.

- **A live Indexing panel** (`mindex.openIndexing`, opened by itself when a reindex
  starts — `mindex.indexingPanel` says where, or `manual` to never). A reindex used
  to report itself through two surfaces that are each structurally one line: a
  notification message and a strip in the corner of the status bar, which a crowded
  bar truncates. Neither could hold what the server actually streams. The panel
  is two blocks and no more: the run's own header — state, clock, phase, a hairline
  bar — and **one list of the files it is working through**, each row carrying its
  language mark, its path and a mark that changes as the file advances. The summary
  arrives underneath when the run ends, and only then: the counters, the embed rate
  with its sparkline, the per-language chunk and symbol table, and a copy button.
  The panel keeps its own clock, so it goes on moving through the long silent
  stretch while a batch is on the GPU.
- **A reindex is now readable as four phases, because it is not a uniform trickle
  of events.** Measured against the live server (14 files, 173 chunks): every
  `prepared` arrives inside 700 ms, then **18.5 seconds of total silence**, then one
  `embedded` already carrying `chunks_done == chunks_total` — the server calls
  `/encode` once per `embed_batch` (256) chunks, so a small run reports its GPU pass
  exactly once, on completion — then every `indexed` and `done` inside 2 ms. So 96%
  of a small run had nothing to draw, which is the hyperjump. The panel now names the
  live phase and times it ("on the GPU — 173 chunks, 12s so far"), and turns the
  progress bar **indeterminate** while the GPU holds the batch rather than freezing
  it at a stale number.
- **Fixed: every counter sat at zero for the whole run and then landed complete.**
  `prepared` already carries each file's final chunk and symbol counts — both are
  written in the same prepare transaction — but they were only counted at `indexed`,
  which arrives after the embed pass. So the numbers had nothing to show for 96% of
  the run and then jumped. They now move as the server reports them, with
  `indexed.count` reconciling the difference rather than adding on top. The average
  rate stays over *embedded* chunks, so the slice phase cannot inflate it.
- **Fixed: nothing rendered at all during the embed pass.** The render was driven
  purely by incoming events, and that stretch sends none — measured 7.8 seconds
  between two consecutive renders, every surface frozen on numbers from before the
  wait. A one-second heartbeat now drives all three surfaces; the panel additionally
  has its own clock.
- **One list, one row per file, updated in place.** An "in flight" list and a "feed"
  meant every path was on screen twice — once looking busy, once looking finished —
  with the same counts printed beside both and a third copy in the metrics block
  above them. There is now a single path-keyed list: a row is created where the
  server first mentions the file and never moves again, and what changes is the mark
  at its end — an outline while it is being sliced, filled while it is on the GPU, a
  green check when it is indexed, a dash when it is skipped, a **red cross** when it
  is not. The row's leading glyph is the language's own mark, not a spinner: every
  file of a batch is in the same `/encode` call, so an animation there pulsed the
  whole list in unison and said nothing. A row's symbol count is omitted at zero
  rather than printed: a language with no tree-sitter tags query — markdown, by
  decision; html, css and the data formats, for want of an upstream one —
  contributes none by construction, and `5 chunks · 0 symbols` on a `.md` file
  reads as a broken counter rather than as the fact it is.
- **A file the run never finished now says so.** A row still in progress when the
  request failed kept its in-progress mark for good — work drawn over a dead
  request. Whatever never settled is now marked by the run's ending: `failed` with
  the error's code, `cancelled` when the user stopped it, and "no result reported"
  in the case the server called the batch done without answering for the file.
- **The panel shows the run from its first frame.** Opening it with no snapshot put
  it into its "nothing has run in this window" state for the whole read-and-post
  stretch: a panel the reindex had just opened by itself claimed no reindex had
  happened.
- **`mindex.batchSize` now defaults to 10, not 100.** Progress is reported per batch,
  and splitting was measured to cost nothing on this stack: 14 files/request took
  18.7 s with 1 progress event, 5 took 17.8 s with 3, 2 took 17.1 s with 7.
- **Fixed: the indexing rate went negative** on any run longer than one batch.
  `chunks_done` is cumulative *per request*, so at every batch boundary it fell
  back toward zero and the sliding window read the drop as backwards progress.
- **Fixed: the last event of a burst was dropped.** The 200 ms render cap fired on
  the leading edge only, so every surface froze one file short of the truth for the
  length of the embed pass — which is exactly the stretch it exists to explain.
- **Fixed: two test suites had never run.** `node --test out/*.test.js` is a
  single-level glob, so everything under `src/shared/` was silently skipped.
- A server that answers without a live stream now says so on the panel, instead of
  degrading in silence.

- **Context is picked from a popup.** An `Add…` button beside the context chips —
  visible in Research mode whether or not anything is picked — opens a QuickPick
  over the stored corpus with server-side search. It offers valid runs only, since
  the server refuses an invalid one as context anyway. `Browse Stored Research`
  (`mindex.browseResearch`) is the single-select twin for reading, and `mindex.research`
  and it both gained keybindings.
- **A stored report opens in its own tab**, as a read-only Markdown document rendered
  by VS Code's own preview — with a provenance header naming the run, its scope, its
  citation verdict and what it was built on. Reachable from the picker, the History
  rows and the dependency chips.
- **A live run shows what it was built on**, as clickable chips in its header; each
  opens that report in a tab.
- **Research History**: batch delete (one request, and the confirmation names how
  many later reports it invalidates), `Ask again` to put a question back in the form
  with its settings and that report as context, visible retention (`pinned`, or a
  countdown in the last week), and the two significance counters — `⤷N` reports this
  one was built on, `↩N` reports built on it.
- The invalid badge now states the **reason** (`3/12 files changed`,
  `context deleted`, `context out of date`) instead of the verdict, and `partial` is
  marked in the warning palette.
- Selecting a row now means *that row*, not *that context*: an out-of-date report is
  exactly what a batch delete is for, so its checkbox is enabled and
  `Use as context` is what refuses instead.
- **Fixed**: deleting the open report left it rendered in the right pane with the
  selection pointing at a dead id; the pane now resets and remembers what was open
  across a reload. A report the server did not save is now called out above the text,
  instead of silently never appearing in History.

## 1.0.1

Research reports become a navigable, reusable knowledge corpus.

- **Research History** (`mindex.openResearchHistory`): an editor-area panel over
  the project's stored research runs — debounced search (titles, questions and
  report bodies), keyset paging, pin/unpin against the retention sweep, delete,
  and a Markdown reader for the selected report.
- **Reports as context**: multi-select runs in Research History and hand them to
  the Ask form as removable chips; the next Research question is answered with
  those reports injected as prior context (`context_run_ids`).
- **Validity surfaced end to end.** Each run shows the server's derived verdict:
  its own staleness (`N/M moved`), an `invalid` badge when the run — or any
  report in its transitive context chain — went stale or was deleted, and a
  `⤷N` marker listing the ancestry. A validity filter joins the freshness one;
  invalid runs cannot be picked as context (the server would refuse them with
  `validation.research_context_invalid`), and an already-picked chip that went
  invalid is marked in the error palette.
- **Titles**: the list shows the report's own stored heading when the server has
  one, falling back to the question.
- Cosmetic UI/UX rework of the sidebar views, language marks and status panel.

## 1.0.0

Initial release: drift view with selective reindex, semantic search QuickPick,
Research (SSE) with live thinking and rendered reports, server status panel,
`.mindex` template, language marks.
