use crate::scanner::{detect_language, Language};
use globset::GlobSet;
use notify::event::{EventKind, ModifyKind, RenameMode};
use std::path::{Path, PathBuf};

/// Raw filesystem event before scope filtering.
pub enum RawEvent {
    Modify(PathBuf),
    Delete(PathBuf),
}

/// Scope-filtered, deduplication-ready event stored in the debounce buffer.
pub enum PendingEvent {
    Upsert { rel: String, lang: Language },
    Delete { rel: String },
}

impl PendingEvent {
    pub fn rel(&self) -> &str {
        match self {
            Self::Upsert { rel, .. } | Self::Delete { rel } => rel,
        }
    }
}

/// Translate a `notify` event into zero or more `RawEvent`s.
pub fn convert_event(event: notify::Event) -> Vec<RawEvent> {
    match event.kind {
        EventKind::Create(_) => event.paths.into_iter().map(RawEvent::Modify).collect(),
        EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) => {
            event.paths.into_iter().map(RawEvent::Modify).collect()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            event.paths.into_iter().map(RawEvent::Delete).collect()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            event.paths.into_iter().map(RawEvent::Modify).collect()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // paths[0] = old name (deleted), paths[1] = new name (created)
            let mut result = Vec::new();
            let mut it = event.paths.into_iter();
            if let Some(from) = it.next() {
                result.push(RawEvent::Delete(from));
            }
            if let Some(to) = it.next() {
                result.push(RawEvent::Modify(to));
            }
            result
        }
        EventKind::Remove(_) => event.paths.into_iter().map(RawEvent::Delete).collect(),
        _ => vec![],
    }
}

/// Apply scope filtering to a raw event and return the `PendingEvent` to enqueue,
/// or `None` if the path is out of scope.
pub fn classify(
    raw: RawEvent,
    root: &Path,
    include_set: &Option<GlobSet>,
    exclude_set: &Option<GlobSet>,
    lang_filter: &[String],
) -> Option<PendingEvent> {
    let (path, is_delete) = match raw {
        RawEvent::Modify(p) => (p, false),
        RawEvent::Delete(p) => (p, true),
    };

    let rel_raw = path.strip_prefix(root).ok()?;
    let rel = rel_raw.to_string_lossy().replace('\\', "/");
    let rel_path = Path::new(rel.as_str());

    if let Some(excl) = exclude_set {
        if excl.is_match(rel_path) {
            return None;
        }
    }
    if let Some(incl) = include_set {
        if !incl.is_match(rel_path) {
            return None;
        }
    }

    if is_delete {
        // Language check is skipped for deletes: the file is gone.
        return Some(PendingEvent::Delete { rel });
    }

    let lang = detect_language(&path)?;
    if !lang_filter.is_empty() && !lang_filter.iter().any(|l| l == lang.name()) {
        return None;
    }

    Some(PendingEvent::Upsert { rel, lang })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mindex_file::build_globsets;
    use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind};

    fn ev(kind: EventKind, paths: &[&str]) -> notify::Event {
        let mut e = notify::Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        e
    }

    fn kinds(events: &[RawEvent]) -> Vec<(bool, &Path)> {
        // (is_delete, path) pairs, so assertions read naturally.
        events
            .iter()
            .map(|e| match e {
                RawEvent::Modify(p) => (false, p.as_path()),
                RawEvent::Delete(p) => (true, p.as_path()),
            })
            .collect()
    }

    #[test]
    fn create_and_data_modify_become_upserts() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Any),
        ] {
            let out = convert_event(ev(kind, &["/r/a.rs"]));
            assert_eq!(kinds(&out), vec![(false, Path::new("/r/a.rs"))], "{kind:?}");
        }
    }

    #[test]
    fn remove_and_rename_from_become_deletes() {
        for kind in [
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
        ] {
            let out = convert_event(ev(kind, &["/r/a.rs"]));
            assert_eq!(kinds(&out), vec![(true, Path::new("/r/a.rs"))], "{kind:?}");
        }
    }

    #[test]
    fn rename_both_deletes_old_and_upserts_new() {
        let out = convert_event(ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &["/r/old.rs", "/r/new.rs"],
        ));
        assert_eq!(
            kinds(&out),
            vec![
                (true, Path::new("/r/old.rs")),
                (false, Path::new("/r/new.rs"))
            ],
            "a rename must delete the OLD path and (re)index the NEW one"
        );
    }

    #[test]
    fn irrelevant_kinds_produce_nothing() {
        for kind in [
            EventKind::Access(notify::event::AccessKind::Any),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        ] {
            assert!(convert_event(ev(kind, &["/r/a.rs"])).is_empty(), "{kind:?}");
        }
    }

    // ── classify: scope filtering ────────────────────────────────────────────

    const ROOT: &str = "/repo";

    fn classify_upsert(
        path: &str,
        include: &[&str],
        exclude: &[&str],
        langs: &[&str],
    ) -> Option<PendingEvent> {
        let (inc, exc) = build_globsets(
            &include.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &exclude.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        classify(
            RawEvent::Modify(PathBuf::from(path)),
            Path::new(ROOT),
            &inc,
            &exc,
            &langs.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn classify_yields_root_relative_forward_slash_paths() {
        let ev = classify_upsert("/repo/src/a.rs", &[], &[], &[]).expect("in scope");
        match ev {
            PendingEvent::Upsert { rel, lang } => {
                assert_eq!(rel, "src/a.rs", "must match the indexed spelling exactly");
                assert_eq!(lang, Language::Rust);
            }
            PendingEvent::Delete { .. } => panic!("a Modify must classify as Upsert"),
        }
    }

    #[test]
    fn classify_drops_paths_outside_the_root() {
        assert!(classify_upsert("/elsewhere/a.rs", &[], &[], &[]).is_none());
    }

    #[test]
    fn classify_exclude_wins_over_include() {
        assert!(
            classify_upsert("/repo/tools/x.rs", &["**/*.rs"], &["tools/**"], &[]).is_none(),
            "an excluded path must be dropped even when the include set matches it"
        );
        assert!(classify_upsert("/repo/src/x.rs", &["**/*.rs"], &["tools/**"], &[]).is_some());
    }

    #[test]
    fn classify_include_set_restricts_scope() {
        assert!(classify_upsert("/repo/docs/a.rs", &["src/**"], &[], &[]).is_none());
        assert!(classify_upsert("/repo/src/a.rs", &["src/**"], &[], &[]).is_some());
    }

    #[test]
    fn classify_language_filter_applies_to_upserts_only() {
        assert!(classify_upsert("/repo/a.py", &[], &[], &["rust"]).is_none());
        assert!(classify_upsert("/repo/a.rs", &[], &[], &["rust"]).is_some());
        // Unknown extension: nothing to index.
        assert!(classify_upsert("/repo/README.md", &[], &[], &[]).is_none());
    }

    #[test]
    fn classify_delete_skips_language_and_extension_checks() {
        // The file is gone — it must be deletable even if its extension is unknown
        // or its language is filtered out (the index may still hold it).
        for path in ["/repo/gone.md", "/repo/gone.py"] {
            let ev = classify(
                RawEvent::Delete(PathBuf::from(path)),
                Path::new(ROOT),
                &None,
                &None,
                &["rust".to_string()],
            )
            .expect("deletes must pass through");
            assert!(matches!(ev, PendingEvent::Delete { .. }));
        }
    }
}
