# Changelog

## Unreleased

Research becomes a first-class surface rather than a panel you have to know about.

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
