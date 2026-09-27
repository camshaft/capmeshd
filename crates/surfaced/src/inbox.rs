//! The surface store — the durable, ordered, bounded inbox that IS a surface
//! (DESIGN §10.1: "a surface IS a persistent inbox").
//!
//! A surface lives here, not in a browser tab: state persists whether or not a
//! browser is attached. Every push appends an [`InboxItem`]; the head of the
//! inbox (latest promoted item) is the main view. Durability is on-disk: each
//! surface is an append-only NDJSON log under the state directory, replayed on
//! startup, so a surface survives a `surfaced` restart. In-memory the store
//! keeps a bounded ring of the most recent items; the on-disk log is the
//! authority for history (log compaction is a later increment).
//!
//! Live attachments (the browser tabs) subscribe to a per-surface broadcast
//! channel; a push or view change fans out to every attached tab as an
//! [`SurfaceEvent`].

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::item::{DisplayItem, InboxItem};

/// How many recent items each surface keeps resident in memory. The on-disk
/// NDJSON log retains the full history beyond this bound.
pub const DEFAULT_INBOX_CAP: usize = 1000;

/// Broadcast buffer depth per surface (older live events dropped if a slow tab
/// falls this far behind; it recovers via the snapshot on reconnect).
const BROADCAST_DEPTH: usize = 256;

/// A live update fanned out to attached tabs over the broadcast channel.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SurfaceEvent {
    /// The full current state, sent first on attach.
    Snapshot { surface: SurfaceView },
    /// A newly pushed item.
    Item { item: InboxItem },
    /// The main view changed to a given item id (or none).
    View { current_view: Option<String> },
}

/// A read-only snapshot of a surface for the wire (REST `GET` + SSE snapshot).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SurfaceView {
    pub id: String,
    pub title: String,
    pub current_view: Option<String>,
    pub items: Vec<InboxItem>,
}

/// In-memory + broadcast state for one surface.
struct SurfaceState {
    id: String,
    title: String,
    items: VecDeque<InboxItem>,
    current_view: Option<String>,
    /// Optional per-surface attach token (DESIGN §10.1). When set, HTTP access to
    /// this surface requires it; `None` means the surface is open. Set via the
    /// (local-trust) control socket, never over HTTP.
    attach_token: Option<String>,
    tx: broadcast::Sender<SurfaceEvent>,
}

impl SurfaceState {
    fn new(id: String) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_DEPTH);
        SurfaceState {
            title: id.clone(),
            id,
            items: VecDeque::new(),
            current_view: None,
            attach_token: None,
            tx,
        }
    }

    fn view(&self, cap: usize) -> SurfaceView {
        // Send at most `cap` most-recent items in a snapshot.
        let items: Vec<InboxItem> = self.items.iter().rev().take(cap).rev().cloned().collect();
        SurfaceView {
            id: self.id.clone(),
            title: self.title.clone(),
            current_view: self.current_view.clone(),
            items,
        }
    }

    fn push(&mut self, entry: InboxItem, cap: usize) {
        if entry.promote {
            self.current_view = Some(entry.id.clone());
        }
        self.items.push_back(entry);
        while self.items.len() > cap {
            self.items.pop_front();
        }
    }
}

/// The set of all surfaces this daemon serves, keyed by surface id.
pub struct SurfaceStore {
    inner: Mutex<HashMap<String, SurfaceState>>,
    state_dir: Option<PathBuf>,
    cap: usize,
}

/// A pushed item plus the broadcast handle needed to notify attachments,
/// returned so the caller (HTTP layer) can fan out after releasing the lock.
pub struct Pushed {
    pub entry: InboxItem,
    tx: broadcast::Sender<SurfaceEvent>,
}

impl SurfaceStore {
    /// Create an in-memory-only store (no on-disk persistence).
    pub fn in_memory() -> Self {
        SurfaceStore {
            inner: Mutex::new(HashMap::new()),
            state_dir: None,
            cap: DEFAULT_INBOX_CAP,
        }
    }

    /// Create a store backed by NDJSON logs under `state_dir`, replaying any
    /// existing surfaces found there.
    pub fn with_state_dir(state_dir: impl Into<PathBuf>) -> Result<Self> {
        let state_dir = state_dir.into();
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating state dir {}", state_dir.display()))?;
        let mut store = SurfaceStore {
            inner: Mutex::new(HashMap::new()),
            state_dir: Some(state_dir),
            cap: DEFAULT_INBOX_CAP,
        };
        store.replay()?;
        Ok(store)
    }

    /// Validate a surface id is a safe single path segment (no traversal): it
    /// maps directly to a log filename, so reject anything but
    /// `[A-Za-z0-9._-]+` and the `.`/`..` specials.
    pub fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id != "."
            && id != ".."
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    }

    fn log_path(&self, id: &str) -> Option<PathBuf> {
        self.state_dir
            .as_ref()
            .map(|d| d.join(format!("{id}.jsonl")))
    }

    /// Load every `<id>.jsonl` log under the state dir into memory.
    fn replay(&mut self) -> Result<()> {
        let Some(dir) = self.state_dir.clone() else {
            return Ok(());
        };
        let cap = self.cap;
        let map = self.inner.get_mut().expect("store lock");
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !Self::valid_id(id) {
                warn!("skipping surface log with unsafe id: {}", path.display());
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut state = SurfaceState::new(id.to_string());
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<InboxItem>(line) {
                    Ok(item) => state.push(item, cap),
                    Err(e) => warn!("skipping malformed log line in {}: {e}", path.display()),
                }
            }
            debug!("replayed surface '{id}' with {} items", state.items.len());
            map.insert(id.to_string(), state);
        }
        Ok(())
    }

    fn append_log(&self, id: &str, entry: &InboxItem) {
        let Some(path) = self.log_path(id) else {
            return;
        };
        let mut line = match serde_json::to_vec(entry) {
            Ok(v) => v,
            Err(e) => {
                warn!("serializing inbox item for '{id}': {e}");
                return;
            }
        };
        line.push(b'\n');
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(&line) {
                    warn!("appending to {}: {e}", path.display());
                }
            }
            Err(e) => warn!("opening {} for append: {e}", path.display()),
        }
    }

    /// Get a read-only snapshot, creating the surface if it does not exist.
    pub fn snapshot(&self, id: &str) -> SurfaceView {
        let mut map = self.inner.lock().expect("store lock");
        let state = map
            .entry(id.to_string())
            .or_insert_with(|| SurfaceState::new(id.to_string()));
        state.view(self.cap)
    }

    /// Ensure a surface exists (creating it if absent), optionally setting its
    /// title and attach token. Backs the control-socket `create-surface` method.
    /// A `None` field leaves the existing value unchanged (idempotent re-create).
    pub fn ensure(&self, id: &str, title: Option<String>, attach_token: Option<String>) {
        let mut map = self.inner.lock().expect("store lock");
        let state = map
            .entry(id.to_string())
            .or_insert_with(|| SurfaceState::new(id.to_string()));
        if let Some(t) = title {
            state.title = t;
        }
        if attach_token.is_some() {
            state.attach_token = attach_token;
        }
    }

    /// Decide whether an HTTP request presenting `presented` may access surface
    /// `id`. An absent or token-less surface is open (returns `true`); a
    /// token-protected surface requires an exact match (DESIGN §10.1 attach auth).
    pub fn authorize_attach(&self, id: &str, presented: Option<&str>) -> bool {
        let map = self.inner.lock().expect("store lock");
        match map.get(id).and_then(|s| s.attach_token.as_deref()) {
            None => true,
            Some(expected) => presented == Some(expected),
        }
    }

    /// Subscribe to a surface's live events, returning the current snapshot and
    /// a receiver. The subscription is registered before the snapshot is taken
    /// (both under the store lock), so no push can slip between them.
    pub fn subscribe(&self, id: &str) -> (SurfaceView, broadcast::Receiver<SurfaceEvent>) {
        let mut map = self.inner.lock().expect("store lock");
        let state = map
            .entry(id.to_string())
            .or_insert_with(|| SurfaceState::new(id.to_string()));
        let rx = state.tx.subscribe();
        (state.view(self.cap), rx)
    }

    /// Append a display item to a surface's inbox (creating the surface if
    /// needed). Persists to the on-disk log, updates the in-memory ring, and
    /// returns the entry plus a broadcast handle for the caller to fan out.
    pub fn push(&self, id: &str, item: DisplayItem, promote: bool) -> Pushed {
        let entry = InboxItem {
            id: uuid::Uuid::new_v4().to_string(),
            ts: now_millis(),
            promote,
            item,
        };
        self.append_log(id, &entry);
        let mut map = self.inner.lock().expect("store lock");
        let state = map
            .entry(id.to_string())
            .or_insert_with(|| SurfaceState::new(id.to_string()));
        state.push(entry.clone(), self.cap);
        Pushed {
            entry,
            tx: state.tx.clone(),
        }
    }

    /// Set the main view to `item_id` (which must name an existing item), or to
    /// none. Returns `true` if applied. Not persisted across restart in this
    /// increment (the log replays the last promoted item as the view).
    pub fn set_view(
        &self,
        id: &str,
        item_id: Option<&str>,
    ) -> Option<broadcast::Sender<SurfaceEvent>> {
        let mut map = self.inner.lock().expect("store lock");
        let state = map.get_mut(id)?;
        if let Some(target) = item_id {
            if !state.items.iter().any(|i| i.id == target) {
                return None;
            }
            state.current_view = Some(target.to_string());
        } else {
            state.current_view = None;
        }
        Some(state.tx.clone())
    }
}

impl Pushed {
    /// Fan the pushed item out to every attached tab. Call after the store lock
    /// is released (a broadcast send never blocks on subscribers).
    pub fn broadcast(&self) {
        // A send with no live receivers returns Err; that is expected (a surface
        // with no browser attached) and not a failure.
        let _ = self.tx.send(SurfaceEvent::Item {
            item: self.entry.clone(),
        });
    }
}

/// Fan a view-change out to attached tabs.
pub fn broadcast_view(tx: &broadcast::Sender<SurfaceEvent>, current_view: Option<String>) {
    let _ = tx.send(SurfaceEvent::View { current_view });
}

/// A push request body decoded from JSON: a display item, and whether it
/// promotes to the main view (default: yes — the latest push is the main view).
#[derive(Debug, serde::Deserialize)]
pub struct PushRequest {
    pub item: DisplayItem,
    #[serde(default = "default_promote")]
    pub promote: bool,
}

fn default_promote() -> bool {
    true
}

/// A set-view request body: the item id to show (null/absent clears the view).
#[derive(Debug, serde::Deserialize)]
pub struct ViewRequest {
    #[serde(rename = "item-id", default)]
    pub item_id: Option<String>,
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reject an unsafe surface id early with a clear error (used by the HTTP layer).
pub fn ensure_valid_id(id: &str) -> Result<()> {
    if !SurfaceStore::valid_id(id) {
        bail!("invalid surface id");
    }
    Ok(())
}

/// Load a surface's raw on-disk log lines (for tests / diagnostics).
pub fn read_log(state_dir: &Path, id: &str) -> Result<Vec<Value>> {
    let path = state_dir.join(format!("{id}.jsonl"));
    let text = std::fs::read_to_string(&path)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid log line"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(body: &str) -> DisplayItem {
        DisplayItem::Text {
            body: body.to_string(),
        }
    }

    #[test]
    fn push_appends_and_promotes_main_view() {
        let store = SurfaceStore::in_memory();
        let a = store.push("phone", text("first"), true);
        let b = store.push("phone", text("second"), true);
        let snap = store.snapshot("phone");
        assert_eq!(snap.items.len(), 2);
        assert_eq!(snap.items[0].id, a.entry.id);
        assert_eq!(snap.items[1].id, b.entry.id);
        // Latest promoted push is the main view.
        assert_eq!(snap.current_view.as_deref(), Some(b.entry.id.as_str()));
    }

    #[test]
    fn inbox_only_push_does_not_move_main_view() {
        let store = SurfaceStore::in_memory();
        let shown = store.push("s", text("shown"), true);
        store.push("s", text("waiting"), false);
        let snap = store.snapshot("s");
        assert_eq!(snap.items.len(), 2);
        // The non-promoting push stayed in the feed but did not become the view.
        assert_eq!(snap.current_view.as_deref(), Some(shown.entry.id.as_str()));
    }

    #[test]
    fn set_view_requires_existing_item() {
        let store = SurfaceStore::in_memory();
        let a = store.push("s", text("a"), false);
        assert!(store.set_view("s", Some(&a.entry.id)).is_some());
        assert_eq!(
            store.snapshot("s").current_view.as_deref(),
            Some(a.entry.id.as_str())
        );
        assert!(store.set_view("s", Some("nope")).is_none());
    }

    #[test]
    fn authorize_attach_gates_only_tokened_surfaces() {
        let store = SurfaceStore::in_memory();
        // Absent surface: open.
        assert!(store.authorize_attach("ghost", None));
        // Token-less surface: open.
        store.ensure("open", None, None);
        assert!(store.authorize_attach("open", None));
        // Tokened surface: requires an exact match.
        store.ensure("sec", Some("Secret".to_string()), Some("k".to_string()));
        assert!(!store.authorize_attach("sec", None));
        assert!(!store.authorize_attach("sec", Some("x")));
        assert!(store.authorize_attach("sec", Some("k")));
        // A None token on re-ensure leaves the existing token intact.
        store.ensure("sec", Some("Renamed".to_string()), None);
        assert!(store.authorize_attach("sec", Some("k")));
        assert!(!store.authorize_attach("sec", None));
    }

    #[test]
    fn valid_id_rejects_traversal() {
        assert!(SurfaceStore::valid_id("phone"));
        assert!(SurfaceStore::valid_id("kitchen-tv.2"));
        assert!(!SurfaceStore::valid_id(""));
        assert!(!SurfaceStore::valid_id(".."));
        assert!(!SurfaceStore::valid_id("a/b"));
        assert!(!SurfaceStore::valid_id("../etc/passwd"));
    }

    #[test]
    fn persists_to_disk_and_replays_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = SurfaceStore::with_state_dir(&path).unwrap();
            store.push("phone", text("one"), true);
            store.push("phone", text("two"), false);
            store.push("phone", text("three"), true);
        }
        // On-disk log holds the full history.
        let log = read_log(&path, "phone").unwrap();
        assert_eq!(log.len(), 3);

        // A fresh store over the same dir replays the surface end-to-end.
        let reborn = SurfaceStore::with_state_dir(&path).unwrap();
        let snap = reborn.snapshot("phone");
        assert_eq!(snap.items.len(), 3);
        assert_eq!(snap.items[2].item, text("three"));
        // The main view is the last promoted item after replay.
        assert_eq!(snap.current_view, Some(snap.items[2].id.clone()));
    }

    #[test]
    fn bound_evicts_oldest_in_memory_but_log_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SurfaceStore::with_state_dir(dir.path()).unwrap();
        store.cap = 3;
        for i in 0..5 {
            store.push("s", text(&format!("m{i}")), false);
        }
        let snap = store.snapshot("s");
        assert_eq!(snap.items.len(), 3);
        assert_eq!(snap.items[0].item, text("m2"));
        assert_eq!(read_log(dir.path(), "s").unwrap().len(), 5);
    }
}
