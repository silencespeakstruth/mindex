# tools/hooks — optional git hooks

Opt-in git hooks that keep a mindex index live as you work. **Off by default** —
nothing here runs unless you install it, so other contributors are unaffected.

## `post-commit` — reindex on commit

After each commit, reindexes the files that commit added/modified and soft-deletes
the ones it removed, so search keeps matching the tree without a manual reindex.
Renames are handled both ways (index the new path, delete the old). It is
best-effort: a post-commit hook runs *after* the commit, so any failure (mindex
down, etc.) is a warning, never a blocked commit.

### Install (pick one, from the repo root)

```sh
# A) symlink just this hook (leaves any other .git/hooks alone)
ln -s ../../tools/hooks/post-commit .git/hooks/post-commit

# B) point git at this whole directory (replaces the hooks dir entirely)
git config core.hooksPath tools/hooks
```

### Requirements

- **`mindex-index` on `PATH`** — `cargo install --path tools/indexer`, or symlink
  the release binary. (Absent → the hook warns and skips; it never blocks a commit.)
- **A repo-root `.mindex`** — GUID on the first non-comment line. An optional
  `exclude_paths:` line scopes the reindex the same way the initial index was
  scoped (e.g. `exclude_paths: tools/**`), so the hook doesn't pull in paths the
  project deliberately excludes.
- **mindex running** at `$MINDEX_SERVER`.

### Config (same `MINDEX_*` conventions as the other tools)

| Variable           | Default                   | Meaning                                  |
| ------------------ | ------------------------- | ---------------------------------------- |
| `MINDEX_SERVER`    | `https://127.0.0.1:11111` | mindex server URL                        |
| `MINDEX_NO_VERIFY` | *(off)*                   | truthy → skip TLS verification           |

### Relation to `drift`

The hook is the *write* side (keep the index live automatically); the MCP `drift`
tool and `mindex-index --check` are the *read* side (report divergence on demand).
If you don't install the hook, you can still catch drift manually with
`mindex-index --check` or the `drift` MCP tool, then reindex.
