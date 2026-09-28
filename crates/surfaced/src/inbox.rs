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

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};

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
    /// The main view changed to a given item id (or none). The field is renamed
    /// explicitly: `rename_all` on an enum renames the *variants*, not the fields
    /// inside a struct variant, so without this the wire key would be
    /// `current_view` while the snapshot (from `SurfaceView`) uses `current-view`
    /// — the page reads `current-view` and would see the live update as null.
    View {
        #[serde(rename = "current-view")]
        current_view: Option<String>,
    },
}

/// A read-only snapshot of a surface for the wire (REST `GET` + SSE snapshot).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SurfaceView {
    pub id: String,
    pub title: String,
    pub current_view: Option<String>,
    /// Live browser attachments (open SSE streams) right now.
    pub attach_count: usize,
    pub items: Vec<InboxItem>,
}

/// A one-line summary of a surface for listings (the MCP `list_surfaces` tool).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SurfaceSummary {
    pub id: String,
    pub title: String,
    pub item_count: usize,
    /// Live browser attachments (open SSE streams) right now.
    pub attach_count: usize,
    pub current_view: Option<String>,
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
    /// Lines appended to the on-disk log since the last compaction. When it
    /// crosses the cap, the log is rewritten to just the retained window so it
    /// does not grow unbounded (the inbox is bounded — DESIGN §10.1).
    appends_since_compaction: usize,
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
            appends_since_compaction: 0,
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
            // Live attachments = active broadcast receivers (one per open SSE stream).
            attach_count: self.tx.receiver_count(),
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

    /// Path of a per-surface state file `<id>.<ext>` under the state dir. Returns
    /// `None` when there is no state dir, or — defense-in-depth — when the id is
    /// not path-safe, so a bad id can never escape the state dir on a write even
    /// if a caller forgot to validate it (every network entry point already does).
    fn state_file(&self, id: &str, ext: &str) -> Option<PathBuf> {
        if !Self::valid_id(id) {
            return None;
        }
        self.state_dir
            .as_ref()
            .map(|d| d.join(format!("{id}.{ext}")))
    }

    fn log_path(&self, id: &str) -> Option<PathBuf> {
        self.state_file(id, "jsonl")
    }

    /// Path of the per-surface view sidecar. The item log is append-only and only
    /// records item pushes, so the main-view selection (which the item log cannot
    /// express) is persisted separately here. Its extension is not `jsonl`, so
    /// replay never mistakes it for the item log.
    fn view_path(&self, id: &str) -> Option<PathBuf> {
        self.state_file(id, "view")
    }

    /// Persist the current main view for a surface, so a selection survives a
    /// restart (the item log alone would replay the last *promoted* item). Best
    /// effort: a write failure is logged, not fatal.
    fn write_view(&self, id: &str, current_view: Option<&str>) {
        let Some(path) = self.view_path(id) else {
            return;
        };
        match serde_json::to_vec(&current_view) {
            Ok(buf) => {
                if let Err(e) = std::fs::write(&path, &buf) {
                    warn!("writing view sidecar {}: {e}", path.display());
                }
            }
            Err(e) => warn!("serializing view for '{id}': {e}"),
        }
    }

    /// Path of the per-surface attach-token sidecar (extension not `jsonl`, so
    /// replay ignores it as an item log).
    fn token_path(&self, id: &str) -> Option<PathBuf> {
        self.state_file(id, "token")
    }

    /// Persist a surface's attach token so a protected surface stays protected
    /// across a restart (in memory only, it would silently reopen). `None`
    /// removes the sidecar (an open surface leaves nothing on disk). Best effort.
    fn write_token(&self, id: &str, token: Option<&str>) {
        let Some(path) = self.token_path(id) else {
            return;
        };
        match token {
            Some(t) => {
                if let Err(e) = std::fs::write(&path, t.as_bytes()) {
                    warn!("writing token sidecar {}: {e}", path.display());
                }
            }
            // Cleared: remove the sidecar rather than leave an empty/stale token.
            None => {
                if path.exists()
                    && let Err(e) = std::fs::remove_file(&path)
                {
                    warn!("removing token sidecar {}: {e}", path.display());
                }
            }
        }
    }

    /// Path of the per-surface title sidecar (extension not `jsonl`, so replay
    /// ignores it as an item log).
    fn title_path(&self, id: &str) -> Option<PathBuf> {
        self.state_file(id, "title")
    }

    /// Persist a surface's title so a friendly name survives a restart (in memory
    /// only, the title would revert to the surface id). Best effort.
    fn write_title(&self, id: &str, title: &str) {
        let Some(path) = self.title_path(id) else {
            return;
        };
        if let Err(e) = std::fs::write(&path, title.as_bytes()) {
            warn!("writing title sidecar {}: {e}", path.display());
        }
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
            // The item log replays the last *promoted* item as the view; a saved
            // sidecar (an explicit selection) overrides it. Honor `null` (no
            // selection) as recorded; ignore a selection whose item has since
            // fallen out of the retained window, keeping the log-derived view.
            // (Read via the local `dir` — `map` holds a `&mut self.inner` here.)
            let saved_view = std::fs::read_to_string(dir.join(format!("{id}.view")))
                .ok()
                .and_then(|t| serde_json::from_str::<Option<String>>(t.trim()).ok());
            match saved_view {
                Some(Some(item_id)) if state.items.iter().any(|i| i.id == item_id) => {
                    state.current_view = Some(item_id);
                }
                Some(None) => state.current_view = None,
                _ => {}
            }
            // Restore the attach token so a protected surface stays protected
            // across a restart (its presence, not contents, is the key fact).
            if let Ok(tok) = std::fs::read_to_string(dir.join(format!("{id}.token"))) {
                state.attach_token = Some(tok);
            }
            // Restore the friendly title (otherwise it would revert to the id).
            if let Ok(title) = std::fs::read_to_string(dir.join(format!("{id}.title"))) {
                state.title = title;
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

    /// Rewrite a surface's on-disk log to exactly `items` (the retained window),
    /// atomically via a temp file + rename, so the append-only log does not grow
    /// unbounded. A crash mid-rewrite leaves either the old log or a stray `.tmp`
    /// (ignored on replay — its extension is not `jsonl`). Called under the store
    /// lock, so no append can interleave and be lost.
    fn compact_log(&self, id: &str, items: &VecDeque<InboxItem>) {
        let Some(path) = self.log_path(id) else {
            return;
        };
        let mut buf = Vec::new();
        for entry in items {
            match serde_json::to_vec(entry) {
                Ok(v) => {
                    buf.extend_from_slice(&v);
                    buf.push(b'\n');
                }
                Err(e) => {
                    warn!("serializing inbox item during compaction of '{id}': {e}");
                    return;
                }
            }
        }
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, &buf) {
            warn!("writing compaction temp {}: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!("renaming compaction temp to {}: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
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

    /// List every registered surface as a summary, sorted by id. Backs the MCP
    /// `list_surfaces` tool (and any registry view). `item-count` is the retained
    /// in-memory window.
    pub fn list_surfaces(&self) -> Vec<SurfaceSummary> {
        let map = self.inner.lock().expect("store lock");
        let mut out: Vec<SurfaceSummary> = map
            .values()
            .map(|s| SurfaceSummary {
                id: s.id.clone(),
                title: s.title.clone(),
                item_count: s.items.len(),
                attach_count: s.tx.receiver_count(),
                current_view: s.current_view.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
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
            self.write_title(id, &state.title);
        }
        if attach_token.is_some() {
            state.attach_token = attach_token;
            self.write_token(id, state.attach_token.as_deref());
        }
    }

    /// Clear a surface's inbox: drop all items, reset the main view, truncate the
    /// on-disk log, and broadcast a fresh (empty) snapshot to attached tabs.
    /// Returns `false` if the surface does not exist (a no-op).
    pub fn clear(&self, id: &str) -> bool {
        let mut map = self.inner.lock().expect("store lock");
        let Some(state) = map.get_mut(id) else {
            return false;
        };
        state.items.clear();
        state.current_view = None;
        state.appends_since_compaction = 0;
        if let Some(path) = self.log_path(id)
            && path.exists()
            && let Err(e) = std::fs::write(&path, b"")
        {
            warn!("truncating log {}: {e}", path.display());
        }
        // The inbox is empty, so the view is none — persist that.
        self.write_view(id, None);
        let _ = state.tx.send(SurfaceEvent::Snapshot {
            surface: state.view(self.cap),
        });
        true
    }

    /// Remove a single item from a surface's inbox by id (prune a stale item
    /// without clearing the whole surface). Rewrites the on-disk log to the
    /// remaining window and broadcasts a fresh snapshot so attached tabs drop it.
    /// If the removed item was the main view, the view resets to none. Returns
    /// `false` if the surface or the item does not exist (a no-op).
    pub fn remove_item(&self, id: &str, item_id: &str) -> bool {
        let mut map = self.inner.lock().expect("store lock");
        let Some(state) = map.get_mut(id) else {
            return false;
        };
        let before = state.items.len();
        state.items.retain(|i| i.id != item_id);
        if state.items.len() == before {
            return false; // no such item — nothing changed
        }
        // If the removed item was the main view, there is nothing to show.
        if state.current_view.as_deref() == Some(item_id) {
            state.current_view = None;
            self.write_view(id, None);
        }
        // Rewrite the log to the retained window (drops the removed item on disk).
        self.compact_log(id, &state.items);
        state.appends_since_compaction = 0;
        let _ = state.tx.send(SurfaceEvent::Snapshot {
            surface: state.view(self.cap),
        });
        true
    }

    /// Remove a surface entirely (from memory and its on-disk log). Returns
    /// `false` if it did not exist. Attached tabs' event streams end (the
    /// broadcast sender drops) and reconnect against a fresh, empty surface.
    pub fn delete(&self, id: &str) -> bool {
        let mut map = self.inner.lock().expect("store lock");
        let removed = map.remove(id).is_some();
        if let Some(path) = self.log_path(id)
            && path.exists()
            && let Err(e) = std::fs::remove_file(&path)
        {
            warn!("removing log {}: {e}", path.display());
        }
        // Drop the view sidecar too, so a re-created surface of the same id does
        // not inherit the deleted surface's selection.
        if let Some(path) = self.view_path(id)
            && path.exists()
            && let Err(e) = std::fs::remove_file(&path)
        {
            warn!("removing view sidecar {}: {e}", path.display());
        }
        // And the token sidecar, so a re-created surface of the same id is open
        // (not silently protected by the deleted surface's token).
        self.write_token(id, None);
        // And the title sidecar, so a re-created surface starts with its id as
        // the title rather than inheriting the deleted surface's name.
        if let Some(path) = self.title_path(id)
            && path.exists()
            && let Err(e) = std::fs::remove_file(&path)
        {
            warn!("removing title sidecar {}: {e}", path.display());
        }
        removed
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

    /// Set (rotate) or clear an existing surface's attach token: `Some` requires
    /// that token for HTTP attachment henceforth, `None` reopens the surface.
    /// Returns `false` if the surface does not exist (create it first). Set only
    /// over the local-trust control socket, never over HTTP (DESIGN §10.1).
    pub fn set_token(&self, id: &str, token: Option<String>) -> bool {
        let mut map = self.inner.lock().expect("store lock");
        match map.get_mut(id) {
            Some(state) => {
                state.attach_token = token;
                self.write_token(id, state.attach_token.as_deref());
                true
            }
            None => false,
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
        let mut map = self.inner.lock().expect("store lock");
        let state = map
            .entry(id.to_string())
            .or_insert_with(|| SurfaceState::new(id.to_string()));
        state.push(entry.clone(), self.cap);
        // Persist under the lock (serialized with compaction, so an append can
        // never be lost to a concurrent rewrite).
        self.append_log(id, &entry);
        state.appends_since_compaction += 1;
        // Compact once we have appended a capful of lines: the file then holds at
        // most ~2×cap lines between compactions, and exactly the retained window
        // after each (which is what a fresh replay would load anyway).
        if self.state_dir.is_some() && state.appends_since_compaction >= self.cap {
            self.compact_log(id, &state.items);
            state.appends_since_compaction = 0;
        }
        // A promoting push moved the main view; persist it so a restart restores
        // this item as the view rather than reverting to the last promoted one.
        if promote {
            self.write_view(id, state.current_view.as_deref());
        }
        Pushed {
            entry,
            tx: state.tx.clone(),
        }
    }

    /// Set the main view to `item_id` (which must name an existing item), or to
    /// none. Returns the broadcast handle if applied. The selection is persisted
    /// to the view sidecar, so it survives a restart (rather than reverting to
    /// the last promoted item, which is all the item log can express).
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
        self.write_view(id, state.current_view.as_deref());
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
    fn store_never_writes_outside_the_state_dir_for_a_bad_id() {
        // state dir is nested one level down; a `../` traversal from it would
        // land in `root/` (still inside the tempdir, so this test can't litter
        // the real filesystem even if the guard regresses).
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let store = SurfaceStore::with_state_dir(&state).unwrap();

        // Directly drive the store with a traversal id (bypassing the entry-point
        // validation) — the store's own path guard must refuse to write.
        store.push("../escape", text("x"), true);
        store.ensure("../escape", Some("t".into()), Some("tok".into()));
        store.set_token("../escape", Some("tok2".into()));

        // Nothing escaped the state dir (no `root/escape.*`).
        for ext in ["jsonl", "view", "token", "title"] {
            let escaped = root.path().join(format!("escape.{ext}"));
            assert!(!escaped.exists(), "traversal wrote {}", escaped.display());
        }
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
    fn view_event_serializes_current_view_in_kebab_case() {
        // The page reads `current-view`; the snapshot (via SurfaceView) uses it,
        // so the live `view` event MUST match or a click reads it as null.
        let ev = SurfaceEvent::View {
            current_view: Some("abc".to_string()),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["kind"], "view");
        assert_eq!(v["current-view"], "abc");
        assert!(
            v.get("current_view").is_none(),
            "must not use snake_case key"
        );
    }

    #[test]
    fn title_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = SurfaceStore::with_state_dir(&path).unwrap();
            store.ensure("s", Some("My Phone".to_string()), None);
            store.push("s", text("hi"), true);
        }
        // A fresh store restores the friendly title (not the surface id).
        let reborn = SurfaceStore::with_state_dir(&path).unwrap();
        assert_eq!(reborn.snapshot("s").title, "My Phone");
    }

    #[test]
    fn attach_token_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = SurfaceStore::with_state_dir(&path).unwrap();
            store.ensure("s", None, Some("secret".to_string()));
            store.push("s", text("hi"), true);
        }
        // A fresh store keeps the surface protected — it must NOT silently reopen.
        let reborn = SurfaceStore::with_state_dir(&path).unwrap();
        assert!(!reborn.authorize_attach("s", None), "must stay protected");
        assert!(!reborn.authorize_attach("s", Some("wrong")));
        assert!(reborn.authorize_attach("s", Some("secret")));
    }

    #[test]
    fn cleared_token_does_not_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = SurfaceStore::with_state_dir(&path).unwrap();
            store.ensure("s", None, Some("secret".to_string()));
            store.push("s", text("hi"), true);
            // Reopen the surface: the token sidecar should be gone.
            assert!(store.set_token("s", None));
        }
        assert!(
            !path.join("s.token").exists(),
            "token sidecar should be removed"
        );
        let reborn = SurfaceStore::with_state_dir(&path).unwrap();
        assert!(
            reborn.authorize_attach("s", None),
            "should be open after clear"
        );
    }

    #[test]
    fn set_view_selection_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let first_id;
        {
            let store = SurfaceStore::with_state_dir(&path).unwrap();
            let a = store.push("phone", text("one"), true);
            first_id = a.entry.id.clone();
            store.push("phone", text("two"), true);
            // Explicitly select the FIRST (older, non-promoted-latest) item.
            assert!(store.set_view("phone", Some(&first_id)).is_some());
        }
        // A fresh store restores the explicit selection, not the last promoted one.
        let reborn = SurfaceStore::with_state_dir(&path).unwrap();
        assert_eq!(reborn.snapshot("phone").current_view, Some(first_id));
    }

    #[test]
    fn explicit_none_view_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = SurfaceStore::with_state_dir(&path).unwrap();
            store.push("phone", text("one"), true);
            // Deselect: the view is none even though an item exists.
            assert!(store.set_view("phone", None).is_some());
        }
        let reborn = SurfaceStore::with_state_dir(&path).unwrap();
        let snap = reborn.snapshot("phone");
        assert_eq!(snap.items.len(), 1);
        assert_eq!(snap.current_view, None);
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

    #[test]
    fn on_disk_log_is_compacted_and_still_replays_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SurfaceStore::with_state_dir(dir.path()).unwrap();
        store.cap = 3;
        // Far more pushes than the cap: without compaction the log would be 50
        // lines; compaction bounds it to ~2×cap.
        for i in 0..50 {
            store.push("s", text(&format!("m{i}")), false);
        }
        let lines = read_log(dir.path(), "s").unwrap().len();
        assert!(lines <= 2 * store.cap, "log not compacted: {lines} lines");

        // The in-memory window stays bounded to the cap, newest first-class.
        let snap = store.snapshot("s");
        assert_eq!(snap.items.len(), 3);
        assert_eq!(snap.items.last().unwrap().item, text("m49"));

        // A fresh store replays the compacted log and preserves the newest item.
        let reborn = SurfaceStore::with_state_dir(dir.path()).unwrap();
        let rsnap = reborn.snapshot("s");
        assert_eq!(rsnap.items.len(), lines);
        assert_eq!(rsnap.items.last().unwrap().item, text("m49"));
    }

    #[test]
    fn clear_empties_the_inbox_and_truncates_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = SurfaceStore::with_state_dir(dir.path()).unwrap();
        store.push("s", text("a"), true);
        store.push("s", text("b"), false);
        assert!(store.clear("s"));
        let snap = store.snapshot("s");
        assert_eq!(snap.items.len(), 0);
        assert_eq!(snap.current_view, None);
        assert_eq!(read_log(dir.path(), "s").unwrap().len(), 0);
        // Clearing an unknown surface is a no-op.
        assert!(!store.clear("nope"));
    }

    #[test]
    fn remove_item_prunes_one_and_rewrites_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = SurfaceStore::with_state_dir(dir.path()).unwrap();
        let a = store.push("s", text("a"), true); // a is the current view
        store.push("s", text("b"), false);
        let a_id = a.entry.id.clone();

        // Remove the viewed item: it drops from the inbox, the log, and the view.
        assert!(store.remove_item("s", &a_id));
        let snap = store.snapshot("s");
        assert_eq!(snap.items.len(), 1);
        assert_eq!(snap.items[0].item, text("b"));
        assert_eq!(
            snap.current_view, None,
            "removing the viewed item clears the view"
        );
        assert_eq!(read_log(dir.path(), "s").unwrap().len(), 1);

        // Removing a nonexistent item, or from an unknown surface, is false.
        assert!(!store.remove_item("s", "does-not-exist"));
        assert!(!store.remove_item("ghost", &a_id));
        // A fresh store replays exactly the remaining item.
        let reborn = SurfaceStore::with_state_dir(dir.path()).unwrap();
        assert_eq!(reborn.snapshot("s").items.len(), 1);
    }

    #[test]
    fn delete_removes_the_surface_and_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = SurfaceStore::with_state_dir(dir.path()).unwrap();
        store.push("s", text("a"), true);
        assert!(dir.path().join("s.jsonl").exists());
        assert!(store.delete("s"));
        assert!(!dir.path().join("s.jsonl").exists());
        assert!(store.list_surfaces().is_empty());
        assert!(!store.delete("s"));
    }

    #[test]
    fn attach_count_tracks_live_subscribers() {
        let store = SurfaceStore::in_memory();
        store.ensure("s", None, None);
        assert_eq!(store.snapshot("s").attach_count, 0);
        let (_view, rx) = store.subscribe("s");
        assert_eq!(store.snapshot("s").attach_count, 1);
        assert_eq!(store.list_surfaces()[0].attach_count, 1);
        drop(rx);
        assert_eq!(store.snapshot("s").attach_count, 0);
    }
}
