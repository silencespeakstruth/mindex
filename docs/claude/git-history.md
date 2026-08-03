# Git history channel — full design record

Companion to `.claude/CLAUDE.md` (condensed invariants there). Read before
modifying `tools/indexer/src/git.rs` or the `/history` endpoints.


The working tree says what the code **is**; `project_commits` +
`project_commit_paths` say **why**. Opt-in (`mindex-index --history`, off by
default), metadata-only: **no embeddings, no Qdrant, no chunks, no derivation
version**, one model-facing tool (`file_history`). **Commit metadata is the
whole feature** — the high-value history questions are SQL questions, not
similarity questions — and the tool is offered, never imposed.

**Semantic search over commit messages is not part of it; the cost is why**:
commit points cannot live in the project's collection (isolation is a `has_id`
filter over `project_file_chunks`), so it means a second collection per project
(doubling the `COLLECTION_SCHEMA_VERSION` hazard), inverting the hard-delete
lifecycle in three places, a derivation version of its own (a sha cannot notice
a changed message-composition rule), and an embed phase in `POST /history`
(~78 GPU batches for a first reconciliation of 20 000 commits, in a request
that today returns in milliseconds). If message *search* is ever wanted, the
ladder is `LIKE` → FTS5 → vectors, each rung measured insufficient first (FTS5
is unusually cheap here: messages are immutable, replaced wholesale). One
defect to solve first for a text-keyed commit tool: a message hit shows the
model **no file**, so its `shown` evidence is empty and the run lands in the
ungrounded-report gate's *exemption*.

**Not pseudo-files, and `/drift` is the reason.** A commit as a
`project_files` row would be reported `orphaned` by every drift check forever,
`--check` would exit non-zero on a clean tree, the watcher would keep trying
to delete it — the `research_runs` argument. Own tables also exclude commit
rows from `build_search_query`'s candidate set **by construction** — why this
is two tables, not a `channel` column. Pinned by
`commit_rows_are_invisible_to_drift` and
`test_commit_paths_never_surface_in_drift`.

**`project_commit_paths.path` carries no FK, deliberately.** A commit names
paths deleted years ago, paths `.mindex` excludes, languages the enum lacks:
`RESTRICT` would refuse the insert, `CASCADE` would erase history when
`prune_deleted_files` runs. The join into the code channel is a *soft* join by
equality; `file_history` must report an un-indexed path as such (the
`outline.indexed` failure again).

**Hard delete, no GC.** These rows own nothing outside SQLite — the
`project_file_symbols` lifecycle. Inverts if messages ever gain vectors.

**Sync is set reconciliation, and that is the whole design.**
`POST /v0/{guid}/history` is a **full-set replace within `since`**: a sha is
the hash of its own content, so there is no update path; force-push, rebase
and rewrite are just reconciliations in which many shas orphan at once.
`since` bounds only the **deletion** half and is load-bearing: without it a
client walking a window would wipe everything older on every pass (an
unmentioned commit and one outside the walk look identical server-side). The
posted set goes through a temp table, not `NOT IN (?, …)` (bind count would
hit SQLite's variable limit within `max_history_commits`).

**Retention is `DELETE /v0/{guid}/history` — the half reconciliation
structurally cannot do.** A `POST` drops only what the tracked refs no longer
reach, so a commit still on `master` never ages out: the age window bounds
*ingestion*, not retention (easy to misread). Bounds: `keep_last=N` (newest by
`committed_at`, `sha DESC` tie-break so a rebase's same-second commits prune
reproducibly) and `older_than=<unix seconds>`, and they **intersect** — a
commit dies only if both condemn it, so `keep_last` is a floor the clock
cannot cut through. Naming neither is a 400
(`validation.history_bound_missing`) — a wipe is asked for (`keep_last=0`),
never arrived at by forgetting a parameter. Deliberately **operator-facing,
called by no client** (a retention flag on `mindex-index` would make every
ordinary run a potential deleter). Destructive without being lossy — the
repository is the source of truth; the next `--history-only` run refills
whatever the refs still reach.

**One producer: `mindex-index`.** Rule 10 (**Four clients**) does **not** fire
— its trigger is what a file set is / what a path spells / what bytes get
hashed, and a commit list is none of those. The watcher, the extension and the
MCP tool are deliberately *not* producers. `--history-only` restricts a run to
the history phase **without switching the channel on** — what lets the
post-commit hook pass it unconditionally. A missing `git` or non-repo root is
a WARN that skips the phase, never a failed run.

**`--relative` is not optional.** `git log --raw` reports paths relative to
the **repository** root while `--root` may be a subdirectory: without it the
soft join is empty for every file and `file_history` answers "no commit
touches this" with nothing erroring. At the repo root it is a no-op; below it,
it also drops commits touching nothing under `--root` (the right scoping).
Pinned by `the_walk_asks_git_for_root_relative_paths`.

**Four traps in `git log --format=<sep> --raw -M -z`**, each pinned by a test
in `tools/indexer/src/git.rs`: `%s` is **not** requested (it is the first
paragraph of `%B` joined; asking for both invites disagreement on a wrapped
subject — derive it). `-z` plus `%x1e`/`%x1f` is mandatory (a body contains
newlines and may contain anything; a body containing `\x1f` costs that one
commit its paths only — records split on `\x1e` first). **The raw block's
arity depends on its status letter**: a rename/copy emits *two* paths — a
parser assuming one desynchronises the rest of the stream. And **git separates
the format output from the diff with a newline**, so the first raw header
arrives as `"\n:100644 …"`: a `starts_with(':')` test without trimming returns
**no paths at all**, for every commit, silently (a merge legitimately has no
paths — that is what makes it silent; it shipped past eight unit tests whose
fixture was tidier than git's real bytes). `old_path`'s biconditional
validation catches the desync at the edge: `Some` on a modification → 400.

**Four client-side drops, all announced**: age **and** count bounds together
(one alone breaks on a repo idle for a year, or having a furious month);
messages under `history_min_message_bytes`; merge commits whose subject is
git-generated **and** body empty **and** >1 parent (the conjunction spares a
GitHub squash-merge — single-parent, carries the PR description, often the
best prose in a repo); commits whose paths all fall outside the project's
globs. An over-cap message is **truncated with a marker**, not dropped (the
server would 400 the whole reconciliation; dropping takes the path list with
it). A channel that quietly indexes a third of what it walked is
indistinguishable from a small repository.

**`file_history` reports three flags because an empty list has three
meanings** (`history_indexed` / `in_scope` / `path_indexed`) — a bare `[]`
reads as the one that is never true. Path-keyed, so out of scope is an
explicit **refusal**. Its `shown` evidence is **only the asked path,
span-less** (recording the commit's other paths would mark files the model
never saw as shown, promoting a later invented citation to `path_only`). **No
commit citation grammar, deliberately** — a sha is content-addressed and
`git show` verifies it; the prompt requires a historical claim be anchored to
a `path:start-end` with the sha named in prose, repeated on every result. A
report citing only shas parses to `total: 0` and correctly trips the
ungrounded gate. Shipping the tool without its `system_prompt` paragraph would
repeat the markdown lesson.

