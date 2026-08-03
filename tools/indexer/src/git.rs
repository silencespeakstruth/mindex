//! The git-history producer: resolve the tracked refs, walk their commits, and
//! turn each into the record the server stores.
//!
//! `mindex-index` is the **only** producer of history, deliberately. The
//! "four clients, one working-tree view" rule in CLAUDE.md exists because four
//! implementations answer "what files are in this project"; nothing here answers
//! that question, so replicating a git walk into the watcher, the extension and
//! the MCP server would add the very surface that rule was written to shrink.
//!
//! We shell out rather than link a git library, following the MCP `drift` tool's
//! precedent. The output format is not casual: `-z` is mandatory because a
//! commit body contains newlines and may contain anything else, and the raw
//! block's arity depends on its status letter — see [`parse_log`].

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use globset::GlobSet;

use crate::client::{ChangeType, CommitEntry, CommitPath};

/// Record separator injected into `--format`. `\x1e` and `\x1f` are the ASCII
/// record/unit separators — chosen because a commit message can legitimately
/// contain newlines, tabs, and any printable byte, so no ordinary delimiter is
/// safe. (A message containing these two is possible but would have to be
/// authored on purpose; `parse_log` degrades to a malformed record it skips.)
const REC: char = '\x1e';
const FIELD: char = '\x1f';

/// Client-side ceiling on one commit's message, matching the server's
/// `[limits].max_commit_message_bytes` default.
///
/// Over-cap messages are **truncated with a visible marker**, not dropped: the
/// server rejects an oversized message with a 400 that fails the whole
/// reconciliation, and a client must never post what the server would refuse
/// (the same rule that makes the scanner drop over-cap files before hashing).
/// Dropping the commit instead would take its path list with it, which is the
/// half that joins history to the code channel.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

const TRUNCATION_MARKER: &str = "\n\n[… commit message truncated by mindex-index …]";

/// What a walk found, plus what it deliberately left out. The counts are
/// reported rather than kept quiet: a history channel that silently drops a
/// third of its commits looks identical to one whose repository is that small.
#[derive(Debug, Default)]
pub struct Walk {
    pub commits: Vec<CommitEntry>,
    /// Lower bound (`committed_at`) of the window this walk speaks for; `None`
    /// when no age bound applied. Sent as the request's `since` so the server
    /// deletes only inside it.
    pub since: Option<i64>,
    pub skipped_short_message: usize,
    pub skipped_generated_merge: usize,
    pub skipped_out_of_scope: usize,
    pub truncated_messages: usize,
}

/// Expand the configured ref patterns into concrete ref names.
///
/// `HEAD` is passed through rather than looked up: it is not a pattern under
/// `refs/heads/` and is the sensible default for a repository nobody has
/// configured. Everything else is matched against local branches — a pattern
/// that matches nothing simply contributes no refs, which is why the caller
/// treats an empty result as "nothing to walk" rather than an error.
pub fn resolve_refs(root: &Path, patterns: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        if pattern == "HEAD" {
            if seen.insert(pattern.to_string()) {
                out.push(pattern.to_string());
            }
            continue;
        }
        let output = Command::new("git")
            .current_dir(root)
            .args([
                "for-each-ref",
                "--format=%(refname:short)",
                &format!("refs/heads/{pattern}"),
            ])
            .output()
            .context("cannot run `git for-each-ref` (is git installed?)")?;
        if !output.status.success() {
            anyhow::bail!(
                "git for-each-ref failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let name = line.trim();
            if !name.is_empty() && seen.insert(name.to_string()) {
                out.push(name.to_string());
            }
        }
    }
    Ok(out)
}

/// Walk the commits reachable from `refs`, bounded by age and count, and return
/// them filtered and ready to post.
///
/// Both bounds apply and the stricter one binds. That is not belt-and-braces: an
/// age bound alone indexes nothing on a repository idle for a year, and a count
/// bound alone reaches back a fortnight on one having a furious month.
#[allow(clippy::too_many_arguments)]
pub fn walk(
    root: &Path,
    refs: &[String],
    max_age_days: Option<u64>,
    max_commits: usize,
    min_message_bytes: usize,
    includes: Option<&GlobSet>,
    excludes: Option<&GlobSet>,
    now: i64,
) -> Result<Walk> {
    if refs.is_empty() {
        return Ok(Walk::default());
    }

    let format = format!(
        "--format={REC}%H{FIELD}%an{FIELD}%ae{FIELD}%at{FIELD}%ct{FIELD}%P{FIELD}%B{FIELD}"
    );
    let mut args: Vec<String> = vec![
        "log".into(),
        format,
        "--raw".into(),
        "-M".into(),
        "-z".into(),
        // Paths must be spelled the way the CODE channel spells them, or the soft
        // join between the two is silently always empty. `git log --raw` reports
        // paths relative to the REPOSITORY root, while `--root` may be a
        // subdirectory of it — so `--root src/` would index `db/qdrant.rs` as a
        // file and `src/db/qdrant.rs` as a commit path, and `file_history` would
        // answer "no commit touches this" for every file in the project.
        // `--relative` makes them relative to the working directory instead,
        // which is `--root`, and drops commits that touched nothing under it.
        // At the repository root it is a no-op.
        "--relative".into(),
    ];
    if let Some(days) = max_age_days {
        args.push(format!("--since={days}.days.ago"));
    }
    args.push(format!("-n{max_commits}"));
    args.extend(refs.iter().cloned());

    let output = Command::new("git")
        .current_dir(root)
        .args(&args)
        .output()
        .context("cannot run `git`; install it or turn the history channel off")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // The overwhelmingly likely cause, and worth naming rather than passing
        // git's own wording through: history is opt-in, so someone turned it on
        // for a tree that is not a checkout.
        if stderr.contains("not a git repository") {
            anyhow::bail!(
                "{} is not a git repository, so there is no history to index. \
                 Point --root at a checkout, or turn the channel off \
                 (--no-history / `history = false`).",
                root.display()
            );
        }
        anyhow::bail!("git log failed: {}", stderr.trim());
    }

    // Lossy on purpose: a repository may hold a commit message in any encoding,
    // and one undecodable byte in one message must not cost the whole walk.
    let text = String::from_utf8_lossy(&output.stdout);
    let mut walk = parse_log(&text);

    // The window the request speaks for. Computed from the same bound `--since`
    // used, with a day of slack: the server deletes only inside `since`, so
    // erring wide would delete commits this walk never looked at.
    walk.since = max_age_days.map(|d| now - ((d + 1) as i64) * 86_400);

    let mut kept = Vec::with_capacity(walk.commits.len());
    for mut c in std::mem::take(&mut walk.commits) {
        // 1. A message this short carries no information but still occupies one
        //    of `file_history`'s slots on every lookup that touches it.
        if c.subject.len() + c.body.len() < min_message_bytes {
            walk.skipped_short_message += 1;
            continue;
        }
        // 2. A generated merge message says nothing the topology does not.
        //    Deliberately a conjunction: a GitHub squash-merge is SINGLE-parent
        //    and carries the pull request's description in its body, which is
        //    often the most valuable prose in a repository.
        if c.parent_count > 1 && c.body.trim().is_empty() && is_generated_merge_subject(&c.subject)
        {
            walk.skipped_generated_merge += 1;
            continue;
        }
        // 3. Keep only the paths this project claims, then drop a commit that
        //    touched none of them — it belongs to a neighbouring tree, not this
        //    project.
        let before = c.paths.len();
        c.paths
            .retain(|p| path_in_scope(&p.path, includes, excludes));
        if before > 0 && c.paths.is_empty() {
            walk.skipped_out_of_scope += 1;
            continue;
        }
        // 4. Truncate rather than let the server refuse the whole batch.
        if c.subject.len() + c.body.len() > MAX_MESSAGE_BYTES {
            truncate_message(&mut c);
            walk.truncated_messages += 1;
        }
        kept.push(c);
    }
    walk.commits = kept;
    Ok(walk)
}

/// Excludes before includes, exactly as the file scanner evaluates them, so a
/// commit's paths are scoped by the same rules as the files themselves.
fn path_in_scope(path: &str, includes: Option<&GlobSet>, excludes: Option<&GlobSet>) -> bool {
    if let Some(ex) = excludes
        && ex.is_match(path)
    {
        return false;
    }
    match includes {
        Some(inc) => inc.is_match(path),
        None => true,
    }
}

fn is_generated_merge_subject(subject: &str) -> bool {
    let s = subject.trim_start();
    s.starts_with("Merge branch ")
        || s.starts_with("Merge pull request ")
        || s.starts_with("Merge remote-tracking branch ")
        || s.starts_with("Merge tag ")
}

/// Cut the message down to [`MAX_MESSAGE_BYTES`], keeping the subject whole and
/// saying out loud that the rest is gone.
fn truncate_message(c: &mut CommitEntry) {
    let room = MAX_MESSAGE_BYTES
        .saturating_sub(c.subject.len() + TRUNCATION_MARKER.len())
        .min(c.body.len());
    // Never split a UTF-8 character: walk back to a boundary.
    let mut cut = room;
    while cut > 0 && !c.body.is_char_boundary(cut) {
        cut -= 1;
    }
    c.body.truncate(cut);
    c.body.push_str(TRUNCATION_MARKER);
}

/// Parse `git log --format=<record-separated> --raw -M -z` output.
///
/// Three properties of that format are load-bearing, and each has cost a bug
/// somewhere:
///
/// - **`%s` is not taken.** It is the first *paragraph* of `%B`, joined — so
///   asking for both invites the two to disagree on a message whose subject
///   wraps. The subject is derived here, by git's own definition.
/// - **The raw block's arity depends on its status letter.** An ordinary change
///   emits `:<modes> <blobs> M\0path\0`; a rename or copy emits
///   `R100\0old\0new\0` — *two* paths. A parser that assumes one desynchronises
///   for the whole rest of the stream, silently attributing every later path to
///   the wrong commit.
/// - **A merge has no raw block at all.** `git log --raw` shows no diff for a
///   merge unless asked with `-m`/`-c`, so a merge commit that survives the
///   filters arrives with an empty path list. That is correct — its changes
///   belong to the commits it merged — and not a parse failure.
fn parse_log(text: &str) -> Walk {
    let mut walk = Walk::default();
    for record in text.split(REC).skip(1) {
        let mut fields = record.splitn(7, FIELD);
        let (Some(sha), Some(author_name), Some(author_email), Some(at), Some(ct), Some(parents)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        // The 7th field is `%B` followed by the trailing FIELD, then git's own
        // NUL commit terminator, then the raw block.
        let Some(rest) = fields.next() else { continue };
        let Some((message, raw)) = rest.split_once(FIELD) else {
            continue;
        };

        let (Ok(authored_at), Ok(committed_at)) = (at.parse::<i64>(), ct.parse::<i64>()) else {
            continue;
        };
        let (subject, body) = split_message(message);
        if subject.is_empty() {
            // The server requires a non-empty subject; posting one would fail
            // the whole reconciliation for a commit nobody can read anyway.
            continue;
        }

        walk.commits.push(CommitEntry {
            sha: sha.trim().to_lowercase(),
            author_name: author_name.to_string(),
            author_email: author_email.to_string(),
            authored_at,
            committed_at,
            parent_count: parents.split_whitespace().count(),
            subject,
            body,
            paths: parse_raw_block(raw),
        });
    }
    walk
}

/// Git's own definition of a subject: the first *paragraph*, its lines joined by
/// a space. The body is everything after the blank line that ends it.
fn split_message(message: &str) -> (String, String) {
    let message = message.trim_start_matches('\n');
    let mut lines = message.lines();
    let mut subject_lines = Vec::new();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        subject_lines.push(line.trim());
    }
    let body: Vec<&str> = lines.collect();
    (
        subject_lines.join(" ").trim().to_string(),
        body.join("\n").trim().to_string(),
    )
}

fn parse_raw_block(raw: &str) -> Vec<CommitPath> {
    let mut out = Vec::new();
    // `trim_start` is load-bearing, not defensive: git separates the format
    // output from the diff with a NEWLINE, so the first header arrives as
    // "\n:100644 …" rather than ":100644 …". Without it the very first token
    // fails the ':' check and every commit comes back with no paths at all —
    // silently, since a commit with no paths is legitimate (a merge has none).
    let mut tokens = raw
        .split('\0')
        .map(str::trim_start)
        .filter(|t| !t.is_empty());
    while let Some(token) = tokens.next() {
        if !token.starts_with(':') {
            // Not a header — the stream is not where we think it is. Stop rather
            // than guess: half a commit's paths beat paths attributed to the
            // wrong commit.
            break;
        }
        let Some(status) = token.split_whitespace().next_back() else {
            break;
        };
        let (change_type, takes_two_paths) = match status.as_bytes().first() {
            Some(b'A') => (ChangeType::Added, false),
            Some(b'D') => (ChangeType::Deleted, false),
            Some(b'R') => (ChangeType::Renamed, true),
            Some(b'C') => (ChangeType::Copied, true),
            // M, T (type change) and anything unknown are a modification: the
            // path exists before and after, which is all the schema records.
            Some(_) => (ChangeType::Modified, false),
            None => break,
        };
        let Some(first) = tokens.next() else { break };
        if takes_two_paths {
            let Some(second) = tokens.next() else { break };
            out.push(CommitPath {
                path: second.to_string(),
                change_type,
                old_path: Some(first.to_string()),
            });
        } else {
            out.push(CommitPath {
                path: first.to_string(),
                change_type,
                old_path: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one record in exactly the byte shape `git log` emits, so the test
    /// exercises the separators rather than a convenient approximation.
    fn record(sha: &str, parents: &str, message: &str, raw: &str) -> String {
        format!(
            "{REC}{sha}{FIELD}A U Thor{FIELD}a@b.c{FIELD}100{FIELD}200{FIELD}{parents}{FIELD}{message}{FIELD}\0{raw}"
        )
    }

    #[test]
    fn a_message_with_newlines_and_tabs_round_trips() {
        // The reason `-z` and the ASCII separators are used at all: neither of
        // these bytes may end a field, and both are ordinary in a commit body.
        let message = "subject line\n\nbody with\ta tab\nand a newline\n";
        let log = record(
            &"a".repeat(40),
            "parent",
            message,
            ":100644 100644 aaa bbb M\0src/a.rs\0",
        );
        let walk = parse_log(&log);
        assert_eq!(walk.commits.len(), 1);
        let c = &walk.commits[0];
        assert_eq!(c.subject, "subject line");
        assert_eq!(c.body, "body with\ta tab\nand a newline");
        assert_eq!(c.parent_count, 1);
        assert_eq!(c.paths.len(), 1);
        assert_eq!(c.paths[0].path, "src/a.rs");
        assert_eq!(c.paths[0].change_type, ChangeType::Modified);
    }

    /// A message containing the unit separator itself is the one input this
    /// framing cannot represent. It has to be authored deliberately — 0x1F is
    /// not reachable from a keyboard or an editor — so the question is not how
    /// to support it but how far the damage spreads. Pinned here: the record
    /// loses its body tail and its paths, and **the next commit parses
    /// normally**. Records are split on `\x1e` before fields are, which is what
    /// keeps one malformed message from desynchronising the whole walk.
    #[test]
    fn a_separator_inside_a_message_costs_that_commit_and_no_other() {
        let log = format!(
            "{}{}",
            record(
                &"a".repeat(40),
                "p",
                "subject\n\nbody with a \x1f inside\n",
                ":100644 100644 aaa bbb M\0src/a.rs\0",
            ),
            record(
                &"b".repeat(40),
                "p",
                "second subject\n\nintact body\n",
                ":100644 100644 ccc ddd M\0src/b.rs\0",
            ),
        );
        let walk = parse_log(&log);
        assert_eq!(walk.commits.len(), 2, "both records must still be seen");
        assert_eq!(walk.commits[0].subject, "subject");
        assert!(
            walk.commits[0].paths.is_empty(),
            "a mangled record yields no paths rather than wrong ones"
        );
        assert_eq!(walk.commits[1].subject, "second subject");
        assert_eq!(walk.commits[1].body, "intact body");
        assert_eq!(walk.commits[1].paths[0].path, "src/b.rs");
    }

    /// The desynchronisation trap. A rename emits two NUL-separated paths and
    /// everything else emits one, so a parser that assumes a fixed arity reads
    /// the *next* entry's header as a path and never recovers — every later path
    /// in the stream lands on the wrong commit, with no error anywhere.
    #[test]
    fn a_rename_consumes_two_paths_and_the_stream_stays_aligned() {
        let raw = ":100644 100644 aaa aaa R100\0old/a.rs\0new/a.rs\0\
                   :100644 100644 bbb ccc M\0src/b.rs\0\
                   :000000 100644 000 ddd A\0src/c.rs\0\
                   :100644 000000 eee 000 D\0src/d.rs\0";
        let walk = parse_log(&record(&"b".repeat(40), "p", "subject\n", raw));
        let paths = &walk.commits[0].paths;
        assert_eq!(paths.len(), 4, "arity must not swallow a later entry");
        assert_eq!(paths[0].path, "new/a.rs");
        assert_eq!(paths[0].old_path.as_deref(), Some("old/a.rs"));
        assert_eq!(paths[0].change_type, ChangeType::Renamed);
        assert_eq!(
            (paths[1].path.as_str(), paths[1].change_type),
            ("src/b.rs", ChangeType::Modified)
        );
        assert_eq!(
            (paths[2].path.as_str(), paths[2].change_type),
            ("src/c.rs", ChangeType::Added)
        );
        assert_eq!(
            (paths[3].path.as_str(), paths[3].change_type),
            ("src/d.rs", ChangeType::Deleted)
        );
        assert!(paths.iter().skip(1).all(|p| p.old_path.is_none()));
    }

    /// Git's subject is the first paragraph joined, not the first line — which
    /// is exactly why `%s` is not requested alongside `%B`.
    #[test]
    fn a_wrapped_subject_is_joined_and_the_body_starts_after_the_blank_line() {
        let walk = parse_log(&record(
            &"c".repeat(40),
            "p",
            "a subject that\nwrapped over two lines\n\nthe body\n",
            "",
        ));
        let c = &walk.commits[0];
        assert_eq!(c.subject, "a subject that wrapped over two lines");
        assert_eq!(c.body, "the body");
    }

    /// A merge carries no raw block; an empty path list is the right answer, not
    /// a parse failure.
    #[test]
    fn a_merge_parses_with_no_paths_and_its_parents_are_counted() {
        let walk = parse_log(&record(&"d".repeat(40), "p1 p2", "Merge branch 'x'\n", ""));
        assert_eq!(walk.commits[0].parent_count, 2);
        assert!(walk.commits[0].paths.is_empty());
    }

    #[test]
    fn the_generated_merge_test_spares_a_squash_merge() {
        // A real merge with git's own wording and nothing else: noise.
        assert!(is_generated_merge_subject("Merge branch 'dev' into master"));
        assert!(is_generated_merge_subject(
            "Merge pull request #12 from x/y"
        ));
        // A squash-merge subject — single-parent in practice, and the body holds
        // the pull request description.
        assert!(!is_generated_merge_subject("feat: add the history channel"));
        assert!(!is_generated_merge_subject(
            "Merged the two code paths at last"
        ));
    }

    #[test]
    fn an_over_long_message_is_truncated_on_a_character_boundary_and_says_so() {
        let mut c = CommitEntry {
            sha: "a".repeat(40),
            author_name: String::new(),
            author_email: String::new(),
            authored_at: 0,
            committed_at: 0,
            parent_count: 1,
            subject: "s".into(),
            // Multi-byte, so a naive byte cut would panic.
            body: "é".repeat(MAX_MESSAGE_BYTES),
            paths: vec![],
        };
        truncate_message(&mut c);
        assert!(c.subject.len() + c.body.len() <= MAX_MESSAGE_BYTES);
        assert!(c.body.ends_with(TRUNCATION_MARKER));
    }

    /// The path spelling has to match the CODE channel's, and the two are
    /// produced by completely different code — the scanner walks the filesystem
    /// relative to `--root`, git reports relative to the *repository* root. When
    /// `--root` is a subdirectory the two disagree, `file_history` answers "no
    /// commit touches this" for every file in the project, and nothing errors.
    /// `--relative` is what aligns them; this pins that the flag is still passed.
    #[test]
    fn the_walk_asks_git_for_root_relative_paths() {
        // A real repository is not needed: the failure is a missing argument, and
        // the argument list is what this checks. Running against a non-repo also
        // exercises the message that names the likely cause.
        let err = walk(
            Path::new("/"),
            &["HEAD".to_string()],
            None,
            10,
            0,
            None,
            None,
            0,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a git repository") || msg.contains("git log failed"),
            "unexpected failure: {msg}"
        );
        assert!(
            msg.contains("--no-history") || msg.contains("git log failed"),
            "a non-repo must be told how to turn the channel off: {msg}"
        );
    }

    #[test]
    fn scope_drops_excluded_paths_before_includes_are_consulted() {
        let includes = mindexfile::build_globsets(&["src/**".into()], &[])
            .unwrap()
            .0;
        let excludes = mindexfile::build_globsets(&[], &["src/generated/**".into()])
            .unwrap()
            .1;
        assert!(path_in_scope(
            "src/a.rs",
            includes.as_ref(),
            excludes.as_ref()
        ));
        assert!(!path_in_scope(
            "src/generated/b.rs",
            includes.as_ref(),
            excludes.as_ref()
        ));
        assert!(!path_in_scope(
            "docs/c.md",
            includes.as_ref(),
            excludes.as_ref()
        ));
        // No filters at all means everything, not nothing.
        assert!(path_in_scope("anything.rs", None, None));
    }
}
