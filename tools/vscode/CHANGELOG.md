# Changelog

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
