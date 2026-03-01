use std::path::Path;
use std::sync::mpsc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::policy_engine::{AccessEvent, FsEventKind};

/// Wraps macOS FSEvents (via the `notify` crate) and collects access events.
pub struct FsWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
}

impl FsWatcher {
    /// Watch all given directories and collect file events from any of them.
    pub fn new(dirs: &[impl AsRef<Path>]) -> Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |ev| {
            let _ = tx.send(ev);
        })
        .context("failed to create FS watcher")?;

        for dir in dirs {
            watcher
                .watch(dir.as_ref(), RecursiveMode::Recursive)
                .context("failed to start watching directory")?;
        }

        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Retrieve all file access events since last call. Drains the channel of events since last poll.
    pub fn poll(&self) -> Vec<AccessEvent> {
        let mut events = Vec::new();
        let now = SystemTime::now();
        while let Ok(Ok(ev)) = self.rx.try_recv() {
            let kind = map_event_kind(&ev.kind);
            for path in ev.paths {
                events.push(AccessEvent {
                    path,
                    kind,
                    timestamp: now,
                });
            }
        }
        events
    }
}

fn map_event_kind(kind: &EventKind) -> FsEventKind {
    match kind {
        EventKind::Create(_) => FsEventKind::Create,
        EventKind::Modify(_) => FsEventKind::Modify,
        EventKind::Remove(_) => FsEventKind::Remove,
        EventKind::Access(_) => FsEventKind::Access,
        EventKind::Any | EventKind::Other => FsEventKind::Other,
    }
}
