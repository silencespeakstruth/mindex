use crate::scanner::{Language, detect_language};
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
