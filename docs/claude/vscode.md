# VS Code extension — full design record

Companion to `.claude/CLAUDE.md` (condensed rules there). Read before
modifying `tools/vscode`.

- VS Code (`tools/vscode`): the **Ask** sidebar WebviewView (`askView.ts`) is
  the one entry point for both query modes — a Search/Research segmented
  toggle over a shared box. An *input surface only*: search results stay in
  the QuickPick (live editor preview + Esc restore), research streams into
  its WebviewPanel tab (steps + live thinking + `marked`-rendered report).
  The SSE client is hand-rolled in `api.ts` (no reconnects — a drop is a
  cancel, by contract). Force reindex lives in the Drift view's overflow
  menu.
  **Research History** (`researchRunsPanel.ts` + `webview/runs.ts`) is an
  editor-area panel, not a third sidebar view (the `icons.test.ts` argument).
  **One full-width list and no reading pane**, a debounced search
  (`shared/debounce.ts`, vscode-free so
  `node --test` reaches it; trailing — first-keystroke results would be wrong
  on arrival), keyset paging by `seq`, and a multi-select that arrives in the
  Ask form as removable chips. **One `AbortController`, aborted on every
  keystroke**, and the caller must swallow `AbortError` itself: `api.request`
  *rejects* on abort while `research()` resolves — "fixing" the asymmetry
  would break every caller's ability to tell a cancelled request from an
  empty answer.
  **Research is popup-first; the panel is the deep end.** Picking context is
  a `QuickPick` (`researchContextPick.ts`) opened from an `Add…` button
  visible in Research mode **whether or not anything is picked**
  (hidden-until-populated made the feature undiscoverable);
  There used to be a read-only single-select twin, `browseResearchRuns` /
  `mindex.browseResearch`; it is **gone**. One line per report was too little
  to choose from, so every use of it ended in the History panel anyway, and two
  reading surfaces meant two places for the challenge and trust wording to
  drift apart. The cost is real and worth recording against the popup-first
  rule above: *reading* a stored report now always costs a panel.
  `ctrl+alt+,` opens the panel instead.
  The picker
  offers **valid runs only** (the server 400s an invalid context id; listing
  one defers the refusal to submit time), tracks selection in
  `onDidChangeSelection` rather than `onDidAccept`'s *visible* selection (a
  pick made under an earlier query is not in `items`), and keeps picked rows
  in `items` for that reason. Cancelling returns `undefined`, which is
  **not** an empty array: one leaves the form alone, the other clears it.
  A stored report opens as a **read-only Markdown document**
  (`researchDocs.ts`, scheme `mindex-research`, `markdown.showPreview`), not
  a fourth webview — outline, find and the user's theme come free, and the
  provider serves from the URI alone so a tab survives a window reload. It
  prepends a provenance block (the stored Markdown says what the run
  concluded, nothing about what it was entitled to claim).
  **The reading pane is gone; a row expands in place.** It used to be a
  `minmax(260px, 24rem)` list beside a `marked`-rendered report: on a wide
  screen that is a cramped column of overlapping badges next to acres of
  prose, and the prose was already available — better, with outline, find and
  the user's theme — as the Markdown tab `openReport` opens. So the list takes
  the whole panel and the selected run expands under its own row with what a
  one-line row cannot carry: provenance, ancestry, the files that have moved,
  the verify/challenge/re-ask actions, and `Open in a tab`. **The report is
  deliberately not rendered anywhere in this panel** — re-adding it is
  re-adding the pane. Consequences that must move together: `activeId` means
  *expanded*, not *selected* (the checkbox is what selects, and sharing a
  highlight made a five-row selection look like five open reports); only one
  row is open at a time (`selectRun` closes first) and clicking the open row
  closes it, since otherwise the only way out of an expansion is another
  expansion; `renderDetail` renders into the `<li>` and **drops the answer if
  the row is gone** (a filter moved under an in-flight fetch — there is
  nowhere honest to put it), so the `runs` handler re-asks for the detail
  when it finds `activeId`'s row present but empty, which is what makes a
  restored `activeId` and a refresh work; and the `challengeState` /
  `verification` messages still key on `activeId` and on ids inside the open
  detail, which one-at-a-time is what keeps unambiguous.
  **The filter row is four selects and nothing else.** The `Show` label and
  the `Outdated`/`Partial` preset buttons (and the host's `applyPreset` and
  its `filters` echo) are gone: a select whose first option reads "Any
  freshness" needs no word in front of it, and a preset button beside the
  select that does the same thing is a second, competing way to set one
  filter — which then has to be echoed back so the two do not disagree.
  In the panel: selection means *rows*, not *context* — the checkbox stays
  enabled on invalid rows and `Use as context` is what refuses. The delete
  confirmation names `referenced_by_count` and states it rather than netting
  it against the selection (a summary carries ancestors, never dependants;
  under-reporting in a delete dialog is the wrong way to be wrong). The
  invalid badge shows the **reason**, not the verdict. `removed` carries an
  id *list* so one path serves both deletes, and it must release `activeId`
  when the open report is the one going (removing the row now takes its
  expansion with it, but a live `activeId` would still key a later
  `challengeState` on a run that is gone). **`removed` also ends the garbage-collection
  review**, which is the same rule one level up: the review describes a corpus that no
  longer exists, and left standing it kept the deleted reports on screen still ticked,
  under a `Delete N` that would re-post ids the server had already dropped — while the
  header above it, which `totals` does refresh, read `Collect garbage (0)`. On the host
  side the release is three maps and two fields (`rows`/`summaries`/`selected`,
  `previewed`/`challengeState`) plus `actions.runsDeleted`, because the Ask form's
  context chips are set from this panel and pruned by nothing else — a deleted run
  stayed attached to the next question and came back as a 400 about a click made in
  another panel. **A run finishing elsewhere refreshes the panel too**
  (`notifyRunFinished`, called from `startResearch`/`startChallenge`'s `finally`), and
  it is deliberately *not* the refresh button: an involuntary refresh does **not**
  `dropBulkSelection()`, because that rule is about the filters that defined the
  selection and none of them changed — discarding several hundred chosen ids because a
  background run landed is a worse surprise than a briefly stale count. Pinning
  re-reads the page for the same reason a delete does: it moves `gc_candidates`, so the
  counts line and the `Collect garbage (N)` label describe the corpus as it was.
  The header's refresh button is in-panel, not a
  `webview/title` menu — three contribution surfaces for one button that
  would then sit in the tab bar, away from the filters it re-runs; it
  supersedes any pending keystroke (`search.cancel()`) and re-fetches the
  open row's detail, whose staleness and trust are exactly the numbers that
  move under the panel. The `kind` and `completeness` filter selects are backed by
  server-side query params — filtered inside the cursor-bounded subquery like
  the rest, so a full page still means "there may be more". That is not a
  detail: `Select all` and `Collect garbage` both page this list **to
  exhaustion** and stop on a short page, so a filter applied after the cut
  would advance the cursor while returning fewer rows and quietly truncate the
  selection. It is why `completeness` was added to the server rather than
  tested on `done_reason` in the client.
  **Bulk selection is defined by the filters that built it**, which is why any
  filter change or refresh clears it *wholesale* rather than pruning it row by
  row: a pruned bulk selection is several hundred ids the user can no longer
  see, chosen by a query no longer on screen, still offered to the delete
  button. It is capped by the published `max_delete_ids` (selecting more than
  one call accepts would hand the user a number the delete cannot honour) with
  `MAX_PAGES` as a runaway backstop, and the footer says when it stopped short.
  The confirmation resolves ids through `summaries` — every row the panel has
  ever *fetched*, including ones never rendered — and not through `rows`, the
  rendered page: resolving a bulk selection through `rows` would report `0`
  dependants for everything off screen, which is exactly the under-reporting
  the rule above forbids.
  **`Collect garbage`** proposes the union of invalid / stale / partial /
  inconclusive-challenge runs, with pinned excluded by the **server's**
  `pinned=false` rather than a client test, so the exemption cannot leak and
  the button's count equals the proposal. The review **takes the whole panel**
  while it is up (the list, its footer and the empty note step aside;
  `Cancel` puts them back untouched, expanded row and all) — it is a decision
  about the corpus, not a detail of one row. Not a second webview: that is a
  second copy of the CSP page assembly, the state protocol, the
  `AbortController` discipline and the delete path, for one screen. Not a
  multi-select QuickPick either: it can pre-select items but cannot show *why*
  each row is proposed or that four later reports were built on it — the two
  things a reviewer unchecks a row over — and each row carries a `read` link
  that opens the report as its own **Markdown tab**, the way stored reports open
  everywhere else. Expanding the row underneath was the earlier call and it destroyed
  the screen it was being read for: `preview` closes the review, and for a candidate
  off the loaded page `renderDetail` then found no row and dropped the answer too,
  leaving neither. That is also why `openRun` resolves through `summaries` and not
  `rows` — the proposal comes from a pass that ran to exhaustion, so most of what it
  offers to open is off-page. Each run appears in **one** group
  (its most serious reason) with the others as labels; three checkboxes for
  one report would let an uncheck in one group not stick. The classification
  is client-side, which is safe *here specifically* because that pass runs to
  exhaustion — no inference is drawn from a page's length.
  The counts line is the corpus `totals`: a **fixed denominator** that no
  filter moves, because the number the filters would give is `runs.length`,
  already on screen. It is **four numbers, not two** —
  `N reports · N challenges · N valid · N outdated` — because a corpus is two
  populations (a challenge is *about* another report, not an answer to a
  question) measured on two axes (validity is what the server will accept as
  context; staleness is what has moved since). `challenges`/`stale` are
  optional on the wire and simply absent from the line on a server too old to
  send them, as are the two counts at zero.
  It says **nothing at all** when the corpus is empty or uncountable: the empty
  state below already says it, in the middle of the panel and at 20px, and it
  says *which* empty — "No research yet" versus "Nothing found", discriminated
  by the controls themselves (a query, or any select off `all`), because "ask a
  question" and "widen the filter" are different next moves. `.runs-items:empty`
  gives up its `flex: 1` so that block owns the height it is centred in.
  **Head chrome**: the magnifier is inside the search field (two controls in a
  row where one is decoration reads as two), and the refresh button and spinner
  sit in the search row rather than among the filters — the filter row wraps,
  and a fixed-size icon button on its right edge was clipped. The panel's own
  chrome is the shared `--mx-*` tokens and the shared controls from
  `common.css`; it used to hand-roll a palette out of raw VS Code variables,
  which is how it came to look like a different product from the sidebar it is
  opened out of.
  **Challenge flow** (`challengeFlow.ts` + `startChallenge` in
  `extension.ts`): launching a challenge is a **QuickPick chain** (effort →
  optional model → optional max-seconds), not a form and not an Ask mode —
  the server refuses a question, scope and context on the challenge body
  (`deny_unknown_fields`; all three come from the subject), so there is
  nothing to compose, and popup-first is the codified shape for that. Three
  entry points, one command (`mindex.challengeResearchRun`, three argument
  shapes): the history panel's preview button (passes the summary), the
  streaming panel's post-done button (passes the new `run_id`; suppressed on
  challenge panels — a challenge of a challenge is a server 400), and an
  `editor/title` button on the report tab's *source* view
  (`resourceScheme == mindex-research`; the Markdown preview tab is a webview
  and offers no per-resource title menu — the palette covers it). The
  command re-fetches the detail before the pre-check: kind/valid/trust must
  not be judged on whatever stale shape the caller held. The client
  pre-check (`challengeGuard` in `shared/runsFormat.ts`) mirrors the
  server's two refusals so the button can explain itself; the server stays
  the authority. The stream reuses `ResearchPanel` verbatim (`isChallenge`
  page flag + a tab title naming the subject) and the same single-flight
  handles as `startResearch` — which is what makes degradation-abort and
  Cancel cover challenges for free; the callback block is built by one
  shared `researchCallbacks()` so the two entrances cannot drift. A 429
  names `MINDex: Active Research Runs`.
  **Trust wording lives in `shared/runsFormat.ts`** (vscode-free, tested):
  the wire types keep `kind`/`trust`/`challenge_verdict` as bare `string`
  (`done_reason` precedent — an unknown future value must not become a type
  lie); the unions and narrowing guards live behind that seam, and every
  consumer of the *meaning* goes through them. Inconclusive is never an
  acquittal; unchallenged is silent (a badge on every row is a badge on
  none); a challenge row links its subject from the **server-resolved**
  `challenged_seq`/`challenged_title`, so a `null` now means the subject is
  genuinely deleted rather than merely off the loaded page.
  **The expanded row always states what was said about a report**
  (`challengeStateLine`). The old version rendered a trust badge and a list,
  and fired the lookup only when `trust !== "unchallenged"` — reasoning that
  derived trust already proves the absence of *valid* challenges. Both halves
  were wrong in the same direction. Trust is correctly silent about an
  inconclusive challenge and about one whose own evidence has moved, so a
  report that had been challenged and **refuted** could show nothing at all
  about having been challenged; and the list was filtered client-side out of
  one unfiltered `kind=challenge` page, so anything past that page was simply
  never found. It is now one indexed `challenged_run_id` query per preview,
  fired for every research run whose row is opened, and every state —
  including "never challenged" — gets a sentence. `limit: 2` is deliberate: the server's
  replace rule is verdict-gated, so an inconclusive re-check leaves the
  standing verdict in place and two rows is a real state the line must name
  rather than silently pick from.
  **Re-check, not a second Challenge.** With a challenge standing,
  `challengeGuard` returns `mode: "recheck"` and the button says so — a second
  "Challenge" would misdescribe what pressing it does, because a fresh run now
  *replaces* the standing verdict when it reaches one. The fork
  (`recheckOptions`) offers both and runs neither automatically: "Links only"
  is `GET …/{challenge_id}/verification` on the **challenge** run, captioned as
  such — reading a challenge's provenance as its subject's would be a worse
  confusion than the one this surface exists to fix — and "Fresh run" goes
  through a modal naming the verdict at risk and the fact that an inconclusive
  result leaves it standing.
  **Verification renders two halves separately** (`verificationView`):
  provenance is immutable — `provenance_matches: false` impeaches the
  journal, never the code — while staleness is computed against the index
  now and is the number that moves; the Verify button re-fetches on every
  click for exactly that reason. Pre-v1.3.0 rows get the staleness half
  only, with the why spelled out.
  **Active runs** (`activeRunsPick.ts`) are a palette command + QuickPick,
  fetched on open — not a status-bar item and not a `StatusMonitor` hook: an
  occupied slot is a rare state consulted deliberately (usually right after
  a 429 named the command), and permanent chrome or a per-tick poll is the
  wrong price. Cancel re-fetches rather than splicing (the 204 is idempotent
  and says nothing; a run may linger while its job unwinds, which is the
  honest state).
  **The form offers only what the server confirmed exists**: language
  pickers = the project's `chunks_active > 0` languages, model field = a
  `<select>` over `research.models`, both via `StatusMonitor.refresh()` —
  the one place that already runs at activation, on `.mindex` change, and
  after every reindex/delete (it re-reads `/config` every pass: the model
  list is no longer static). Three rules: `undefined` inventory means
  *unknown* (server down, no project, older server) and falls back to
  `ALL_LANGUAGES`, as does an *empty* one (an empty picker is a dead form; a
  superset merely lets a filter match nothing); the `readScope`/submit
  whitelists stay `ALL_LANGUAGES`, **not** narrowed to the inventory
  (offering is an availability hint, validating is a contract); everything
  is pushed by `postMessage` and rebuilt in the webview, never by
  reassigning `webview.html` (a re-render would discard the half-typed
  question, the restored `getState()` and a live run's Cancel state).
  **The form is also gated on what the server can currently do, and that
  gate needs a clock.** `fetchStatus` publishes one `Availability {ask,
  research, reason}`, split because a *required* dependency takes everything
  down (server reports `unhealthy`) while Ollama takes only Research
  (`degraded`) — one flag would either kill Search whenever no local model
  runs, or keep offering Research against a server that cannot serve it. The
  reason names the *required* checks, never Ollama. **The verdict is read
  from `status` AND `checks` together** (`readHealth`): a server older than
  the tri-state vocabulary says `degraded` for the *required* case, so
  keying on `status` alone would paint yellow and leave the form armed
  against a server with no vector store. Two flags stay two: a third would
  need a control that is live under `unhealthy` and dead under
  Ollama-degradation, and there is none.

  **The Server Status health card is one dot per dependency and a 2×2 block per
  row** (`webview/status.ts` + `CHECK_META`): dot + name + `optional` badge
  top-left, purpose bottom-left, `ok`/`failed` top-right, what that state costs
  bottom-right. Identity stacks on one edge, verdict on the other, and **both
  captions share the grid's second row** — which is what keeps the halves
  aligned when either wraps, and what two stacked flex rows could not do. The glyph is the **same circle
  in every state** — colour is the severity, and swapping it for a warning
  triangle and then an error circle made three unrelated indicators out of one
  thing changing colour; the word beside it (`ok` / `not answering`) is what
  carries the state without colour, which is the job the changing shape was
  doing badly. The captions are the second half of the fix: a standing `purpose`
  clause under the name (3-10 words — "qdrant is not
  answering" means nothing to a reader who does not know what qdrant is for,
  and that used to live only in a tooltip nobody hovers a green row for), and a `cost` clause under the verdict, drawn in
  **both** states (dim and prefixed "otherwise:" when the row is green, in the
  row's colour when it is not — a caption that appeared only on failure made
  every red row taller than the green one above it). The `optional` badge sits
  beside the *name*: it qualifies the dependency permanently and never its
  current answer. Both it and "only Research needs it" used to ride at the end
  of the verdict row, centred against a value column they had nothing to do
  with, reading as part of the verdict. An unlisted check
  falls back to `UNKNOWN_CHECK` and is treated as **required**, which is the
  safe direction to be wrong in.

  **A degradation freezes the form and never the tabs**, and both halves are
  reversals of earlier rules.

  The *tabs* used to disable: a dead Ollama greyed out the Research tab. That
  is a dead end which explains nothing — the sentence saying *why* Research
  is unavailable lives behind the tab the user cannot press. So both tabs are
  live in every state, switching modes is always allowed, and the notice
  inside the mode is the answer. It names the missing dependency **and what
  it costs in this tab** ("…, so Search is unavailable until it recovers"):
  "the server is degraded" is not an answer to "why is my Search button
  dead".

  The *fields* used to stay live, on the argument that composing costs the
  server nothing. In practice a form that accepts text, globs and budget
  changes under a red notice states two contradictory things at once, so the
  gate moved outward: when the mode on screen cannot be served
  (`modeUsable()` — `ask`, plus `research` in the Research tab), every
  `input`/`textarea`/`select`/`button` inside `#form` is disabled through
  `setEnabled`, so the busy layer keeps composing with it. Three exemptions,
  each load-bearing: the **mode switch** (it is how the user reaches the
  other tab's explanation), **Stop** (a run in flight still has a connection
  to drop, and the control that ends it must not be the one that dies) and
  the notices' own **Open Server Status** links (disabling the remedy being
  offered). `#submit` is skipped by the sweep and gated in `render`, which
  also has `running`/`searching` to account for — two writers on one button
  would fight. Anything that *rebuilds* controls (`renderContextRuns`, the
  `languages` message) must re-run the sweep, or a chip born during a freeze
  is the one live control on the form. `canSubmit()` reads the button's own
  `disabled` and is asked at **both** entry points — the click *and* the
  Enter keydown, which used to call `submit()` blind and fire research at a
  dead Ollama. `#scope-folder` posts its own `scopeFolder` message rather
  than a `submit` the host early-returns on: a control that fills in a text
  field has no business on the channel that launches runs, and while it was
  there it was the one unguarded way in. A degradation also aborts what is
  running (via `RunRegistry`), resetting handles **before** reporting (a
  notification's thenable resolves only on dismissal — the trap that once
  left Research disabled behind an un-clicked toast), and reports it as a
  failure, not a cancellation (which would read as the user's own Stop).
  None of this is observable without `[mindex.statusPollSeconds]` (default
  30, `0` = off): every other refresh is event-driven.
  **Language marks are vendored, two-toned and tested.** `esbuild.mjs`
  generates `src/shared/langGlyphs.ts` from devicon's *monochrome* SVGs
  (fills stripped so CSS `color` drives them), committed; `sql` alone falls
  back to a codicon. Each language declares **two** colours in
  `media/lang.css` (13 of 21 brand colours fail 3:1 against one of VS Code's
  default backgrounds; the pair is derived by mixing toward white/black in
  5% steps until it clears — `langIcons.test.ts` recomputes the derivation
  and asserts no mark kept a hard-coded fill). Devicon's *font* was rejected
  on size (1.5 MB vs a 181 KB extension).
  Drift's `Sync all` is a synthetic first tree row present **only** while
  there is actionable drift; it reindexes before deleting, so a failure or
  declined confirm still leaves the index better off. Its explanatory prose
  lives in `viewsWelcome`, not `TreeView.message` (VS Code renders the
  message *instead of* the welcome view when the tree is empty — set it only
  once a check has produced rows).
  **The generated `.mindex` reads the project's `.gitignore` files rather
  than guessing** (`gitignore.ts`, pure, no `vscode` import). The rest of the
  template is guesswork and says so — a `dist/` may be checked-in output —
  but a `.gitignore` is the project's own statement of what is generated, so
  those excludes go in live, blocked per source file. Four things make that
  safe, each of which fails *silently* if dropped. **The Rust parser rejects
  what this one accepted**: `mindexfile::build_globset` bails on a leading
  `/` or any `\`, so gitignore's anchoring and escape syntax must be resolved
  at translation time, never carried. **A `!` must never reach the file** —
  picomatch reads it as negation, globset as a literal, and `.mindex` has no
  negation to express it with; since `include_paths` cannot re-admit what an
  exclude dropped, a negation instead *disarms* (comments out) the positive
  rules it overlaps, decided by materializing it into sample paths. Dropping
  them silently is the jemalloc `.gitignore` case — `/test/unit/[A-Za-z]*`
  plus `!…*.*` would delete every unit test from the index with no error.
  **Both glob forms are emitted** for a pattern that could name a file or a
  directory (`target` → `**/target` *and* `**/target/**`): globs are matched
  against file paths only, so the second is what actually excludes a build
  tree. And **the walk prunes as it goes** — a directory the rules so far
  already exclude is not entered, which is what keeps forty `.gitignore`
  files inside `perf/corpus/.clones/` and `.ruff_cache/.gitignore` (content:
  `*`) out of the result. Anything untranslatable is written as a comment
  naming the pattern and the reason; the empty case renders byte-for-byte
  what it always did, which is both the no-git path and the revert switch.
  Nothing re-reads `.gitignore` afterwards — mindex's scope is the `.mindex`,
  and this happens once.
  **A reindex must show the server's claims, not just its own upload.**
  `post_index` swallows the claim conflict (`Err(ApiError::FileInFlight) =>
  {}`) and still 200s with the claimed file *absent from the response* —
  byte-for-byte a hash-skipped file, so the extension once reported a
  refused reindex as `unchanged`. It is now read from two places, neither
  that response: `/status`'s `indexing_claims` drives a live Drift-view row
  and *refuses* to start an upload that would be swallowed, and the
  follow-up `/drift`'s `indexing` bucket is what the summary subtracts to
  say "still indexing" instead of "unchanged" — so the drift check must run
  **before** the summary. The status poll drops to 3 s while claims are
  outstanding (the configured interval stays the ceiling). Every entry point
  funnels through the one `reindex()` helper — what makes its re-entry guard
  total (two concurrent runs raced their own drift checks and could settle
  showing just-indexed files as stale).
  **The run reports itself as a feed, not a percentage, because indexing is
  batched** (a file-granular bar moves in two bursts with the long stretch
  frozen between them — the `▰▱` row is gone and `increment` is no longer
  reported). What is live is `IndexFeed` (`shared/indexFeed.ts`,
  vscode-free): the last five paths, the counters, and a `RateWindow` over
  the server's **cumulative** `chunks_done` rather than a local sum of
  `batch_chunks` (a retry or batch boundary cannot make the two disagree).
  One snapshot feeds two surfaces, and the split is forced: a `withProgress`
  message is structurally **single-line** (`\n` collapses; no multi-line
  API), so the paths live in a `StatusBarItem`'s `MarkdownString` tooltip
  and the toast keeps the one line and the Cancel button only it can hold.
  The Drift view keeps the **claims** row (other clients' work) and nothing
  else — the re-entry guard moved to a `reindexRunning` flag in `activate()`
  (`isBusy` was derived from the deleted progress state).
  **Every button that reaches the server goes inert until its result lands,
  and the refusal is host-side.** `BusyKeys` (`src/busy.ts`, `vscode`-free,
  `busy.test.ts`) is one single-flight per key; `applyBusy`/`setEnabled`
  (`webview/ui/busy.ts`) paint it onto `[data-busy-key]` elements. Two rules
  it exists to enforce. **Supersede reads, refuse writes**: aborting and
  restarting is right for a keystroke-driven list load, where the newer
  query is the wanted one, and wrong everywhere else — a superseded `more`
  aborted the page it had just asked for and returned early without
  advancing the cursor, so holding the key made paging stop dead, and a
  superseded delete is still a delete. **A disabled button is a courtesy,
  not a guarantee** — a restored panel, a keyboard race or a message already
  in flight can still post — so the greyed state is the *echo* of the host's
  decision, never its cause; two confirmation modals for one row is what the
  other way round allows. `setEnabled` layers busy over a control's **own**
  verdict in a `WeakMap` (and seeds from the authored `disabled` on first
  touch): `#runs-delete` is disabled because nothing is selected, and an
  unrelated key clearing must not enable it. `{type:"loading"}` is retired —
  the runs spinner rides the same `list` key as the buttons, so the two can
  no longer disagree. Keys in use: `list`/`more`/`gc`/`delete`/`preview`/
  `verify`/`row:<id>` (history), `refresh`/`retry`/`row:<path>` (status),
  `submit` (Ask — shared by the form and the `mindex.search` palette
  command, so five fast clicks are one search rather than five quick picks
  racing to open). The status panel's `refresh` key is fed by
  `StatusMonitor.onDidChangeRefreshing`, not by the press, so a *background*
  poll greys it too — otherwise the panel visibly re-renders while the
  button that supposedly caused it sits idle and clickable.
  **No raw error text reaches the user, anywhere.** `humanize(e)`
  (`problem.ts`, table-documented and pinned by `problem.test.ts`) returns
  `{text, retryable, cancelled, code}`; `text` is a sentence and **never
  contains the machine `code`**, which survives on its own field for a
  tooltip and the log. `reportError` and every panel funnel through it, and
  the raw stack goes to a lazily created `MINDex` output channel via
  `logError` — that is what makes the rule affordable rather than
  destructive, since otherwise a bug report contains no error at all. The
  shortcut it replaces is `e.message`, which for a `ProblemError` is
  `code (status): detail`: eight catch sites in the history panel rendered
  `research.not_found (404)` and `connect ECONNREFUSED 127.0.0.1:11111` at
  users. `retryable` decides both the Retry button and the banner's colour
  (yellow "press it again" vs red "this will not work"), and the banner
  survives **exactly until something renders successfully** — a rule in the
  webview's message handler rather than an `ok()` at eight host call sites,
  because forgetting one is how a transient failure came to sit over a list
  that had since loaded. `noteIfLegacy` had to stop reading the rendered
  message and became a shape test (`ProblemError`, 400, `detail` matching):
  matching English for a version check breaks the day the wording improves.
  **Requests have deadlines, and the stream's is idle-only.**
  `mindex.requestTimeoutSeconds` (15, `0` = off) arms **two** clocks per
  request — `req.setTimeout` for socket inactivity, plus a total deadline at
  2×, because a peer dribbling one byte at a time resets the first forever.
  Health polls clamp to `HEALTH_TIMEOUT_MS` (5 s): a poll that outlives its
  own interval stacks, and the busy interval is 3 s. `MindexApi.withTimeout`
  is an `Object.create` view sharing the agent — the poll calls five
  endpoints and all five must be bounded by the *poll's* clock. A timeout is
  its own `TimeoutError`, matched **before** the `UnreachableError` wrap, or
  it reaches the user as "is the server running?" — wrong, and the first
  thing they have already checked. A 2xx body that will not parse is
  `MalformedResponseError`, also not "unreachable": something answered.
  `mindex.streamIdleTimeoutSeconds` (180, `0` = off) is **idle-only, never
  total** — a legitimate `high` run lives up to the server's 70-minute
  ceiling, so any total deadline would eventually kill a working run; the
  number is derived from the server's own 120 s `first_token_timeout_ms` /
  `report_timeout_ms` plus slack. It must not take the `abortResolves` path:
  a silent stream is a failure, the user's Stop is not. The legacy non-SSE
  fallback disarms the clock entirely (a buffered synchronous index over a
  large repo is legitimately minutes of silence). `StatusMonitor` gains a
  single-flight guard, a 20 s backstop deadline and an abort on `dispose` —
  the bug being that `reschedule` re-arms in a `.finally()`, so one
  never-settling poll ended health polling for the life of the window and
  froze the indicator at whatever colour it had.
- MCP `scout` (`tools/mcp/scout/`): token-economy layer, one tool —
- VS Code (`tools/vscode`): `npm run check` = prettier + eslint + `tsc` + the
  `node --test` suite (`src/*.test.ts`, compiled to `dist/`).
- Shell: `shellcheck scripts/entrypoint.sh`, `shellcheck --shell=bash
  tools/search/mindex-search.sh`; format `shfmt -i 4 -ci` (bare shfmt
  defaults to tabs).
- Python (`tests/`): `ruff check`, `ruff format --check` **and**
  `black --check` (kept compatible), `mypy` (`fastapi` is `# type: ignore` —
  stubs only in the mock's image). Run mypy **per directory** — `mypy tests/`
  fails with `Duplicate module named "main"`:
  `for d in tests/integration tests/mock_embedder tests/mock_ollama; do mypy $d; done`.
- Python (MCP servers): the same four, per server —
  `(cd tools/mcp/scout && ruff check . && ruff format --check . && black --check . && mypy src)`,
  likewise for `tools/mcp/mindex`. Easy to forget: neither is under `tests/`.
- SQL: `sqlfluff lint src/db/migrations/` (dialect/layout from repo-root
  `.sqlfluff`; schema is intentionally column-aligned).
- Prefer a scoped `#[allow(...)]`/config exclusion **with a reason** over
  contorting code; never project-wide suppression.

## When modifying code

1. New loops touching Qdrant/SQLite/embedder must respect the
   `CancellationToken`.
2. Multi-row DB writes go inside a `transaction`.
3. New endpoints: register in `backend::http3::run`, use `RouterState`,
   `{param}` routes, `#[debug_handler]`, the `ApiJson`/`ApiPath`/`ApiQuery`
   extractors, return `Result<_, ApiError>`, validate at the top via
   `backend::v0::validate` (new check = new `ApiError` variant + arms +
   `codes_are_stable` + a unit test). Add a `#[utoipa::path]` annotation
   (existing tag, every error `body = ProblemDetails`, a `**Concurrency:**`
   note) **and** an entry in `openapi.rs` `paths(...)` (+ new types in
   `schemas(...)`) — a handler missing there is silently absent from Swagger;
   `openapi_spec_is_complete_and_versioned` guards the count. Swagger UI at
   `/swagger-ui` (assets vendored, no network).
4. Reach Qdrant only via `VectorStore`; collection names via
   `collection_for`.
5. Any search-path SQLite query must include `AND c.status = 'active'`.
6. Status writes use `set_file_status` and must be a legal transition
   (triggers enforce it). New status-changing paths need a transition test.
7. Adding a language → the full checklist under **Languages**.
8. Schema change → new migration in the `MIGRATIONS` slice with the next
   sequential version; startup applies those above `PRAGMA user_version`,
   then stamps it. All SQL `IF NOT EXISTS` (cold re-run = no-op, enforced by
   `every_migration_sql_is_idempotent`). SQLite can't `ALTER` a `CHECK` onto
   an existing table — add new constraints as `BEFORE INSERT/UPDATE` triggers
   (the status-machine pattern, additive). New *columns* are equally blocked:
   `ADD COLUMN` has no `IF NOT EXISTS` form, so it fails the idempotency
   test. **v1.0.0 is frozen** — an in-place edit is skipped in silence on any
   database stamped at 1; first symptom is a 500 with `no such
   table`/`no such column`. New *tables* are the easy case
   (`v1.1.0_git_history.sql`). **Widening a constraint, and adding a column,
   are both answered by the table rebuild** — `v1.1.0_toml_yaml_languages.sql`
   is the precedent; copy its shape: create the replacement under a temporary
   name, copy rows with columns **named** (`SELECT *` binds by position),
   `DROP` the original, rename, recreate its triggers (the `DROP` took them).
   It runs under `SQLite3Pool::migration_transaction` because both halves
   need foreign keys suspended (rename-first makes the children follow the
   discarded table; `ON DELETE RESTRICT` refuses the `DROP`);
   `apply_pending_migrations` pays that back with one
   `PRAGMA foreign_key_check` before stamping. Idempotency comes from the
   leading `DROP TABLE IF EXISTS <tmp>`. Rehearse on a copy of a real
   database and compare row counts per table. **A 1:1 side table is not the
   answer for a new field** (the three that existed each cost a hot-path
   JOIN); `v1.2.0_research_context.sql` is the precedent for rebuilding to
   add columns. One rebuild consequence: the FK suspension also suspends
   `ON DELETE CASCADE`, so dropping the old table does **not** take child
   rows with it — `id` surviving the copy is what makes them still resolve;
   load-bearing, pinned by
   `rebuilding_research_runs_keeps_the_baselines_that_reference_it` (through
   an ordinary transaction the same migration silently erases every child
   row).
9. Changing how chunks or symbols are derived → bump the matching const under
   **Derivation versions** — that is what makes the change reach files
   already indexed; skipping it leaves them stale behind a matching hash,
   silently.
10. Changing which files a project contains, how a path is spelled, what
    bytes are hashed, or which files a client refuses to post → **the full
    list under Four clients, one working-tree view**, in the same commit. One
    client changed alone is not a smaller version of the change; it is
