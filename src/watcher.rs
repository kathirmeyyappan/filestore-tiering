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
    /// Initialize on 'dir', and begin watching it for file-access events.
    pub fn new(dir: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |ev| {
            let _ = tx.send(ev);
        })
        .context("failed to create FS watcher")?;

        watcher
            .watch(dir, RecursiveMode::Recursive)
            .context("failed to start watching directory")?;

        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Retrieve all file access events since last call.
    pub fn poll(&self) -> Vec<AccessEvent> {
        let mut events = Vec::new();
        let now = SystemTime::now();
        // drain channel of all file access events since last time
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
