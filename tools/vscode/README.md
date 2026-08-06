# mindex-vscode

The human half of MINDex: **research, search and index upkeep from the editor.** The
agent-facing tools live under `tools/mcp/`; this extension drives the same API for you.
Nothing runs in the background — every action is explicit.

## What it does

- **Ask** (sidebar → Ask). One box for both ways of querying the index, with a
  Search/Research toggle above it; the option row swaps to match. Enter submits,
  Shift+Enter adds a newline. **Scope** is shared by both modes — language chips plus
  `only`/`never` glob fields, with buttons to take the scope from the current folder or
  from `.mindex` — because the server accepts the same selector for a search and for a
  research run. Both disclosures state their contents in their own header, so
  collapsing them hides the controls and never the decision. Where the answers land
  differs, because the two answers are different shapes:
  - **Search** — a QuickPick in rank order (`#rank score path`, line span, snippet)
    live-previewing each result in the editor underneath. Enter opens it, Esc puts you
    back where you were. Also on `ctrl+alt+/` and `mindex: Search`, which prompt for the
    query so you never have to reach for the sidebar (that entry point is
    deliberately scope-free — there is no form to read a scope from).
  - **Research** — a **local** model does the whole investigation; steps, its live
    thinking and the rendered Markdown report stream into their own tab as they are
    produced. *Stop* — or closing the tab — drops the connection, which **is** the
    server-side cancel. One run at a time. **Budget** overrides the effort preset axis
    by axis on sliders bounded by what the server publishes; an axis you have not
    touched shows the preset greyed out and is not sent at all, and its ⟲ button puts
    it back.
- **Research History** (`ctrl+alt+,`, or *MINDex: Show Research History*). An
  editor-area panel over every report the server has kept: **one full-width list, no
  reading pane**. A selected run expands under its own row with its provenance, the
  reports it was built on, which of its files have moved since, and its actions; the
  report itself opens only as a Markdown tab, so nothing in the panel competes with
  reading it. Debounced search, keyset paging, visible retention (`pinned` / `3d left`),
  and an invalid badge that shows the *reason* rather than the verdict.
  - **Challenge.** Any valid report can be given an opponent — a second run whose
    subject is the first, streamed into the ordinary Research panel. Rows carry the
    verdict as a badge and link challenge to subject in both directions. The expanded
    row always states what was said about a report: trust is correctly silent about an
    inconclusive challenge and about one whose own evidence has since moved, so a report
    that had been challenged and *refuted* would otherwise read as untouched. With a
    challenge standing the button becomes **Re-check**, which forks between re-verifying
    the challenge's own links and a fresh run — the latter behind a modal naming the
    verdict at risk.
  - **Verify** re-scores a report's citations offline: no model, no GPU. Provenance and
    staleness are rendered as two separate answers, because they are — provenance is
    immutable and a mismatch means a journal bug, while staleness is computed against
    the index as it stands now and is the number that moves.
  - **Pruning.** *Select all* pages the server with the current filters to exhaustion,
    and the footer says when it stopped short. *Collect garbage* proposes the union of
    invalid, stale, partial and inconclusive runs — pinned ones exempt — into a review
    that takes the whole panel, each run listed once with all of its reasons, and every
    delete confirmation names how many other reports depend on what you are removing.
- **Active runs** (*MINDex: Active Research Runs*). A QuickPick over what is currently
  holding the server's research slots, oldest first, with cancel. It is what a `429`
  points you at.
- **Drift** (sidebar → Drift). *Check Drift* hashes the working tree (using the `.mindex`
  scope) and buckets it into `stale` / `missing` / `orphaned` / `indexing` as a checkbox
  tree, with selective *Reindex*, *Delete Orphaned*, and *Cancel In-Flight*. Its overflow
  menu also holds the two **force** reindexes (this file / whole project), which ignore
  the server's unchanged-skip. You rarely want them: the skip already compares derivation
  versions, so an ordinary reindex picks up slicer and tags-query changes on its own.
  Force is for what that cannot see — a grammar-crate bump, a suspect index, debugging —
  and re-embeds everything it touches, so the whole-project one asks first.
- **Create .mindex** (Drift view, when there is no project file — welcome button or
  *MINDex: Create a .mindex Project File*). Generates a GUID, writes a commented
  template at the workspace root and opens it. Only root dot-directories that are
  unambiguously tool or VCS state (`.git`, `.venv`, caches) go in as live excludes;
  every other dot-directory found and the usual build-artifact globs ride along
  commented out, since a `dist/` may well be worth indexing and guessing wrong shrinks
  the index in silence. What does *not* need guessing is read instead: every
  `.gitignore` in the project, nested ones included, is translated into `exclude_paths`
  and written in live, each block naming the file it came from. git's pattern language
  is not this one, so the translation is explicit about its edges — a `!` re-inclusion
  has no equivalent here, and the rules it would have re-admitted are commented out
  with the reason rather than applied. Never overwrites an existing file — it opens it
  instead, and nothing re-reads `.gitignore` afterwards.
- **Status bar → Server Status.** The status bar carries one indicator — **MINDex** in
  green, yellow or red — and clicking it refreshes and opens the Server Status panel in
  an editor tab: `/health`, `/status`, this project's per-language inventory, and the
  failed-file dead-letter list with per-file or all-at-once *Retry*. It is a panel and
  not a third sidebar view because it is consulted only when the indicator is not
  green. The health card is **one dot per dependency in every state**, laid out so that
  everything saying *what this is* stacks on one edge and everything saying *how it is
  doing* on the other. The `ollama` check is the server's **optional** dependency: when
  it is down the server answers `degraded` rather than `unhealthy` — indexing and search
  are unaffected and only Research breaks — so it renders as a warning, the status bar
  appends a note, and the Ask view's Research tab shows a notice. A run that fails on it
  re-checks health, so the two agree without a manual refresh.
- **Settings** are one click away from either sidebar view's toolbar and from the
  Server Status panel — which is where an unreachable server sends you, since
  `mindex.serverUrl` / `mindex.noVerify` / `mindex.caCert` are usually the answer.

- **The bearer token** lives in the OS keychain, set by **MINDex: Set Bearer
  Token** — never in a setting, which Settings Sync would copy to every other
  machine. A status-bar entry appears as it nears expiry (`mindex.tokenWarningHours`,
  24 by default) and turns red under an hour or once expired; it is absent while
  the token is healthy, which is what makes its appearance mean something.
  **MINDex: Issue a Token for an Agent** derives a short-lived token for the
  current project and copies it to the clipboard — for pasting into an agent's
  context, which is the one place a credential legitimately goes by hand. Its
  actions are ticked rather than fixed: read and research start on, `index` and
  `delete` are offered off behind a confirmation naming what they cost. It cannot
  widen what the extension's own token holds; the server refuses that, and the
  choices here only keep the dangerous shapes behind a deliberate tick.

- **A read-only token is a supported way to run this extension.** Nothing refuses
  to start. A token without `research` freezes that tab's controls and says so in
  the notice inside it — the tab itself stays live, because the explanation is
  behind it; a token without `index` turns a reindex into one sentence instead of
  a batch of per-file refusals. What the token says is treated as a *hint* about
  what to offer; the server remains what decides. A token minted `--for` another
  kind of client is refused when you paste it, naming both audiences — the server
  does not check that label, which is exactly why the check is here.

- **A project the token does not name** answers 404 for everything, identically
  to a project nobody has ever indexed — deliberately, so nothing can enumerate
  GUIDs. So the extension says so at the one moment it can: right after
  **Create a .mindex Project File** writes a GUID your token does not cover. Mint
  a token naming it on the server's host.

Errors surface as the server's `code — detail`; infra failures offer *Retry*.

## Install

**From a release.** Download `mindex-vscode-<version>.vsix` from the
[releases page](https://github.com/silencespeakstruth/mindex/releases) and install it —
the extension is pure TypeScript, so one file works on every platform VS Code does:

```sh
code --install-extension mindex-vscode-2.0.0.vsix   # then: Developer: Reload Window
```

You can also build that file yourself with `npm install && npm run package`.

**Linked install**, for when you are editing the extension. Point VS Code's extension
directory straight at this folder — then a rebuild is picked up by a window reload,
with no reinstall step at all:

```sh
npm install && npm run compile
ln -s "$PWD" ~/.vscode/extensions/mindex.mindex-vscode-2.0.0
```

Day to day: leave `npm run watch` running, and hit *Developer: Reload Window* after a
change. Keep the link's version suffix in step with `package.json` if you bump it —
VS Code reads the identity from the manifest but expects the directory to be named
`publisher.name-version`.

Either way the reload matters — neither installing a VSIX nor rebuilding restarts a
running extension host. For a throwaway session, open this folder and press F5. Requires VS Code ≥ 1.85; the extension
activates only in workspaces containing a `.mindex` file at a folder root (YAML: a
`guid:` key plus optional `exclude_paths:`/`include_paths:`/`languages:` lists). That
scope drives the drift manifest — keep it accurate, or vendored trees get hashed and
reported `missing`.

The file is watched: creating, editing or deleting it takes effect immediately, no
window reload. If it is missing or malformed the views are disabled and the Drift
view offers to create one; the first workspace folder that has a `.mindex` wins,
and nested ones are ignored.

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `mindex.serverUrl` | `https://127.0.0.1:11111` | Server base URL |
| `mindex.noVerify` | `false` | Skip TLS verification (self-signed cert) |
| `mindex.caCert` | — | PEM CA to trust instead (e.g. the mkcert root CA) |
| `mindex.tokenWarningHours` | `24` | How long before the bearer token expires to start warning in the status bar; `0` disables the early notice, and under an hour it turns red regardless. The **token itself is not a setting** — run **MINDex: Set Bearer Token**, which stores it in the OS keychain, so Settings Sync cannot carry a credential between machines |
| `mindex.researchModel` | — | Pre-fills the Research model field (empty = server default) |
| `mindex.topK` | `10` | Where the Search results slider starts (its ceiling comes from the server) |
| `mindex.batchSize` | `10` | Files per `/index` request |
| `mindex.statusPollSeconds` | `30` | Background health re-check interval; `0` disables it |
| `mindex.requestTimeoutSeconds` | `15` | Deadline for an ordinary request; `0` waits forever. Health polls use a shorter one of their own |
| `mindex.streamIdleTimeoutSeconds` | `180` | How long a streaming research or indexing run may send **nothing at all** before the connection is treated as dead. An idle clock, never a total one — a `high` run may legitimately live 70 minutes |
| `mindex.indexingPanel` | `beside` | Where the live Indexing panel opens |

The health poll is what lets the Ask view stop offering work the server cannot do. The
server answers a **tri-state** verdict and owns it, so the extension does not keep its
own copy of which dependency is required: `ok`; `degraded`, meaning only the optional
Ollama is failing — Research disables itself, search and indexing carry on; and
`unhealthy`, meaning a required dependency is down, which freezes the whole form and
aborts anything that was running, naming which one. A degradation never disables the
mode **tabs** — a disabled tab is a dead end whose explanation lives behind it — so both
stay live in every state and the notice inside the mode says what is missing and what it
costs there. Without a poll none of it is noticed until something else happens to
refresh.

Node ignores the OS trust store, so a mkcert-issued cert needs `mindex.caCert`
(`mkcert -CAROOT`) or `mindex.noVerify`.

Research additionally needs the server's `[research]` section pointed at a local Ollama,
and the embedder running.

The form's bounds — the results ceiling, the effort ladder, the budget ceilings and the
model list — all come from `GET /config` rather than from numbers written here, so they
cannot drift from the server. Against a server too old to publish them the form falls
back to the compiled defaults, and against one with no model list the model field stays
free text rather than becoming an empty dropdown.
