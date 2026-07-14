# mindex-vscode

VS Code extension for a running mindex server (see the repo-root README): on-demand drift
checking, selective reindexing, server status with retry, and semantic search — the
interactive counterpart to `mindex-index` (bulk CLI) and `mindex-watch` (daemon).
Nothing runs in the background: every action is explicit.

## Features

- **Drift view** (activity bar → mindex → Drift). *Check Drift* walks the workspace
  with the `.mindex` scope, hashes every file, and POSTs `/drift`. Results appear in
  four buckets — `stale`, `missing`, `orphaned`, `indexing` — as a collapsible
  directory tree with checkboxes on files and folders (a folder toggles its subtree).
  - *Reindex Selected* posts the checked stale/missing files to `/index` in batches
    (progress + cancel).
  - *Delete Selected Orphaned from Index* soft-deletes checked orphaned paths
    (modal confirmation; GC reclaims vectors later).
  - *Reindex All Stale + Missing* and *Cancel In-Flight Indexing* cover the bulk cases.
- **Server Status view + status bar.** `/health` (sqlite / qdrant / embedder checks),
  `/status` runtime counters, and the failed-file dead-letter list with per-file or
  all-at-once *Retry* (`POST /retry`; the retry worker picks requeued files up within
  ~60 s). The status bar item shows ok / degraded / unreachable and refreshes on click.
- **Search** (`mindex: Search`, `ctrl+alt+/`). Query → a QuickPick lists every
  result in true rank order (score descending, as the server returns them): each
  row shows `#rank score path`, the line span, and a one-line snippet of the
  chunk. Moving through the list live-previews the location in the editor
  underneath; typing filters by path/snippet; Enter opens the result with the
  range selected, Esc closes the list and restores the editor you came from.
- Errors are the server's problem+json rendered as `code — detail` toasts; infra
  failures (unreachable, 503) offer a *Retry* button; cancellations are silent.

## Project identity & scope

The project GUID comes from the repo-root `.mindex` file (same format as
`mindex-watch`): GUID on the first non-comment line, optional `include_paths:` /
`exclude_paths:` / `languages:` comma-lists. The same scope drives both the drift
manifest and reindexing, so keep it accurate — paths not excluded there (e.g. a
vendored `.venv/`) will be hashed, reported `missing`, and uploaded if you reindex
them.

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `mindex.serverUrl` | `https://127.0.0.1:11111` | Base URL of the server |
| `mindex.noVerify` | `false` | Skip TLS verification (self-signed cert) |
| `mindex.caCert` | — | PEM CA to trust instead (e.g. the mkcert root CA) |
| `mindex.topK` | `10` | Search results to request (≤ 100) |
| `mindex.batchSize` | `100` | Files per `/index` request |

Node does not read the OS trust store, so a mkcert-issued server cert needs either
`mindex.caCert` pointed at the mkcert root CA (`mkcert -CAROOT`) or `mindex.noVerify`.

## Build & install

```sh
npm install
npm run compile                                 # tsc → dist/
npx --yes @vscode/vsce package                  # → mindex-vscode-<version>.vsix
code --install-extension mindex-vscode-*.vsix   # then: Developer: Reload Window
```

The reload step matters: installing a VSIX does not restart an already-running
extension host. For development instead, open this folder in VS Code and press F5
(Extension Development Host). Requires VS Code ≥ 1.85 (checkbox tree API).

The extension activates only in workspaces containing a `.mindex` file.
