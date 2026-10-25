//! Bounded in-memory store of capture entries plus live events.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::config::EventCallback;
use crate::entry::CaptureEntry;

/// Live updates for the UI.
#[derive(Clone, Debug)]
pub enum ProxyEvent {
    /// A new entry (usually still pending).
    Added(Arc<CaptureEntry>),
    /// An entry changed (response headers, completion, tunnel/WebSocket progress).
    Updated(Arc<CaptureEntry>),
    /// Entries evicted by the capacity or memory limits.
    Removed(Vec<u64>),
    Cleared,
    Stopped,
}

pub(crate) struct Store {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
    events: broadcast::Sender<ProxyEvent>,
    callback: Option<EventCallback>,
}

struct Inner {
    entries: VecDeque<Arc<CaptureEntry>>,
    capacity: usize,
    max_bytes: usize,
    /// Bytes held by the stored entries.
    bytes: usize,
    /// Body bytes buffered by exchanges that are still streaming (see
    /// [`Store::reserve`]); they count toward `max_bytes` too.
    in_flight: usize,
}

impl Store {
    pub fn new(capacity: usize, max_bytes: usize, callback: Option<EventCallback>) -> Self {
        let (events, _) = broadcast::channel(1024);
        Store {
            inner: Mutex::new(Inner {
                entries: VecDeque::new(),
                capacity: capacity.max(1),
                max_bytes,
                bytes: 0,
                in_flight: 0,
            }),
            next_id: AtomicU64::new(1),
            events,
            callback,
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProxyEvent> {
        self.events.subscribe()
    }

    pub fn emit(&self, event: ProxyEvent) {
        if let Some(cb) = &self.callback {
            cb(&event);
        }
        let _ = self.events.send(event);
    }

    pub fn insert(&self, entry: CaptureEntry) {
        self.put(entry, true, 0);
    }

    /// Replaces the stored entry with the same id. Returns `false` when it is
    /// no longer stored (evicted or cleared).
    pub fn update(&self, entry: CaptureEntry) -> bool {
        self.put(entry, false, 0)
    }

    /// Inserts (`insert`) or replaces `entry` while returning `release`
    /// in-flight bytes (the buffers that now live in the entry) in the same
    /// step, so they are never counted twice.
    pub fn put(&self, entry: CaptureEntry, insert: bool, release: usize) -> bool {
        let entry = Arc::new(entry);
        let evicted = {
            let mut inner = self.inner.lock();
            inner.in_flight = inner.in_flight.saturating_sub(release);
            if insert {
                inner.bytes += entry.stored_bytes();
                // Ids are allocated in order but an entry can be published late
                // (content-type filter), so keep the deque sorted.
                let pos = inner.entries.partition_point(|e| e.id < entry.id);
                inner.entries.insert(pos, entry.clone());
            } else {
                let Ok(pos) = inner.entries.binary_search_by_key(&entry.id, |e| e.id) else {
                    return false;
                };
                let old = std::mem::replace(&mut inner.entries[pos], entry.clone());
                inner.bytes = inner.bytes - old.stored_bytes() + entry.stored_bytes();
            }
            inner.evict(entry.id, false)
        };
        self.emit(if insert { ProxyEvent::Added(entry) } else { ProxyEvent::Updated(entry) });
        if !evicted.is_empty() {
            self.emit(ProxyEvent::Removed(evicted));
        }
        true
    }

    /// Accounts for up to `want` more body bytes that the still-streaming
    /// entry `owner` wants to buffer, evicting entries older than it to make
    /// room (a newer entry outlives an older one, as everywhere else).
    /// Returns how many of them may be kept: fewer than `want` once nothing
    /// older is left to evict.
    pub fn reserve(&self, owner: u64, want: usize) -> usize {
        let (granted, evicted) = {
            let mut inner = self.inner.lock();
            inner.in_flight += want;
            let evicted = inner.evict(owner, true);
            let over = (inner.bytes + inner.in_flight).saturating_sub(inner.max_bytes);
            let granted = want.saturating_sub(over);
            inner.in_flight -= want - granted;
            (granted, evicted)
        };
        if !evicted.is_empty() {
            self.emit(ProxyEvent::Removed(evicted));
        }
        granted
    }

    /// Returns in-flight bytes whose buffers were dropped.
    pub fn release(&self, bytes: usize) {
        let mut inner = self.inner.lock();
        inner.in_flight = inner.in_flight.saturating_sub(bytes);
    }

    pub fn get(&self, id: u64) -> Option<Arc<CaptureEntry>> {
        let inner = self.inner.lock();
        inner.entries.binary_search_by_key(&id, |e| e.id).ok().map(|i| inner.entries[i].clone())
    }

    pub fn list(&self) -> Vec<Arc<CaptureEntry>> {
        self.inner.lock().entries.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn stored_bytes(&self) -> usize {
        self.inner.lock().bytes
    }

    pub fn in_flight_bytes(&self) -> usize {
        self.inner.lock().in_flight
    }

    pub fn clear(&self) {
        {
            let mut inner = self.inner.lock();
            inner.entries.clear();
            inner.bytes = 0;
        }
        self.emit(ProxyEvent::Cleared);
    }
}

impl Inner {
    /// Drops the oldest entries beyond the limits, never `keep` (and with
    /// `older_only` nothing newer than it either). For the byte budget only
    /// entries that hold bytes are dropped (dropping an empty or still-pending
    /// one frees nothing).
    fn evict(&mut self, keep: u64, older_only: bool) -> Vec<u64> {
        let mut evicted = Vec::new();
        while self.entries.len() > self.capacity {
            let idx = if self.entries.front().is_some_and(|e| e.id == keep) { 1 } else { 0 };
            let Some(old) = self.entries.remove(idx) else { break };
            self.bytes -= old.stored_bytes();
            evicted.push(old.id);
        }
        while self.bytes + self.in_flight > self.max_bytes {
            let evictable =
                |e: &Arc<CaptureEntry>| (if older_only { e.id < keep } else { e.id != keep }) && e.stored_bytes() > 0;
            let Some(idx) = self.entries.iter().position(evictable) else { break };
            let Some(old) = self.entries.remove(idx) else { break };
            self.bytes -= old.stored_bytes();
            evicted.push(old.id);
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;
    use bytes::Bytes;

    fn entry(store: &Store, body: usize) -> CaptureEntry {
        let mut e = CaptureEntry::new(store.next_id(), "127.0.0.1:1".parse().unwrap(), EntryKind::Http);
        e.response_body.data = Bytes::from(vec![0u8; body]);
        e
    }

    #[test]
    fn ring_buffer_and_budget() {
        let store = Store::new(3, 100, None);
        let mut rx = store.subscribe();
        for _ in 0..5 {
            let e = entry(&store, 10);
            store.insert(e);
        }
        let ids: Vec<u64> = store.list().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
        assert!(matches!(rx.try_recv(), Ok(ProxyEvent::Added(_))));

        // Memory budget: a big update evicts the oldest others but keeps itself.
        let mut big = (*store.get(4).unwrap()).clone();
        big.response_body.data = Bytes::from(vec![0u8; 85]);
        assert!(store.update(big));
        let ids: Vec<u64> = store.list().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![4, 5]);
        assert_eq!(store.stored_bytes(), 95);

        // Late (out of order) insert stays sorted.
        let store2 = Store::new(10, 1000, None);
        let (e1, e2, e3) = (entry(&store2, 0), entry(&store2, 0), entry(&store2, 0));
        store2.insert(e3);
        store2.insert(e1);
        store2.insert(e2);
        let ids: Vec<u64> = store2.list().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        store2.clear();
        assert_eq!(store2.len(), 0);
        assert!(!store2.update(entry(&store2, 0)));
    }

    #[test]
    fn bodies_in_flight_count_toward_the_budget() {
        let store = Store::new(10, 100, None);
        let old = entry(&store, 60);
        let old_id = old.id;
        store.insert(old);
        // A pending entry holds nothing, so it is never evicted for bytes.
        let pending = entry(&store, 0);
        let id = pending.id;
        store.insert(pending);

        // Buffering 50 bytes of the pending body evicts the finished entry.
        assert_eq!(store.reserve(id, 50), 50);
        assert!(store.get(old_id).is_none());
        assert!(store.get(id).is_some());
        // Beyond the budget only the rest is granted.
        assert_eq!(store.reserve(id, 70), 50);
        assert_eq!(store.reserve(id, 1), 0);
        assert_eq!((store.stored_bytes(), store.in_flight_bytes()), (0, 100));

        // Finishing moves the buffers into the stored entry in one step.
        let mut done = (*store.get(id).unwrap()).clone();
        done.response_body.data = Bytes::from(vec![0u8; 100]);
        assert!(store.put(done, false, 100));
        assert_eq!((store.stored_bytes(), store.in_flight_bytes()), (100, 0));
        // An older body still streaming does not evict this newer entry.
        assert_eq!(store.reserve(old_id, 10), 0);
        assert!(store.get(id).is_some());
        // Clearing keeps the reservations of exchanges still streaming.
        store.clear();
        let other = entry(&store, 0);
        assert_eq!(store.reserve(other.id, 30), 30);
        store.clear();
        assert_eq!(store.in_flight_bytes(), 30);
        store.release(30);
        assert_eq!(store.in_flight_bytes(), 0);
    }
}
