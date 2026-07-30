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
  the index in silence. Never overwrites an existing file — it opens it instead.
- **Status bar → Server Status.** The status bar carries one indicator — **MINDex** in
  green, yellow or red — and clicking it refreshes and opens the Server Status panel in
  an editor tab: `/health`, `/status`, this project's per-language inventory, and the
  failed-file dead-letter list with per-file or all-at-once *Retry*. It is a panel and
  not a third sidebar view because it is consulted only when the indicator is not
  green. The `ollama` check is the server's **optional** dependency: when it is down,
  Health stays `ok` (indexing and search are unaffected) and only Research breaks — so
  it renders as a warning, the status bar appends a note, and the Ask view's Research
  tab shows a notice. A run that fails on it re-checks health, so the two agree without
  a manual refresh.
- **Settings** are one click away from either sidebar view's toolbar and from the
  Server Status panel — which is where an unreachable server sends you, since
  `mindex.serverUrl` / `mindex.noVerify` / `mindex.apiKey` are usually the answer.

Errors surface as the server's `code — detail`; infra failures offer *Retry*.

## Install

**From a release.** Download `mindex-vscode-<version>.vsix` from the
[releases page](https://github.com/silencespeakstruth/mindex/releases) and install it —
the extension is pure TypeScript, so one file works on every platform VS Code does:

```sh
code --install-extension mindex-vscode-1.0.1.vsix   # then: Developer: Reload Window
```

You can also build that file yourself with `npm install && npm run package`.

**Linked install**, for when you are editing the extension. Point VS Code's extension
directory straight at this folder — then a rebuild is picked up by a window reload,
with no reinstall step at all:

```sh
npm install && npm run compile
ln -s "$PWD" ~/.vscode/extensions/mindex.mindex-vscode-1.0.1
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
| `mindex.apiKey` | — | Sent as `X-Api-Key`; only needed behind a reverse proxy |
| `mindex.researchModel` | — | Pre-fills the Research model field (empty = server default) |
| `mindex.topK` | `10` | Where the Search results slider starts (its ceiling comes from the server) |
| `mindex.batchSize` | `100` | Files per `/index` request |
| `mindex.statusPollSeconds` | `30` | Background health re-check interval; `0` disables it |

The health poll is what lets the Ask view stop offering work the server cannot do:
Research disables itself when the server's (optional) Ollama goes away, and the whole
view disables when a *required* dependency does — naming which one, and aborting
anything that was running. Without a poll none of that is noticed until something else
happens to refresh.

Node ignores the OS trust store, so a mkcert-issued cert needs `mindex.caCert`
(`mkcert -CAROOT`) or `mindex.noVerify`.

Research additionally needs the server's `[research]` section pointed at a local Ollama,
and the embedder running.

The form's bounds — the results ceiling, the effort ladder, the budget ceilings and the
model list — all come from `GET /config` rather than from numbers written here, so they
cannot drift from the server. Against a server too old to publish them the form falls
back to the compiled defaults, and against one with no model list the model field stays
free text rather than becoming an empty dropdown.
