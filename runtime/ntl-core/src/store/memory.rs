//! In-memory [`NodeStore`] — the reference implementation.
//!
//! This backend is the semantic baseline every other backend is checked
//! against. It is pure Rust with no dependencies beyond `std`, so it builds
//! everywhere `ntl-core` does, including `wasm32-unknown-unknown`.
//!
//! It is appropriate for tests, for the `--dev` CLI mode, and for genuinely
//! ephemeral edge nodes that re-bootstrap from scratch. It is not
//! appropriate for anything that must survive a restart: see
//! [`Durability::Memory`].

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use super::{
    ActivationSnapshot, Durability, JournalEntry, JournalId, NodeStore, Outcome, PeerRecord,
    PeerSource, StoreError, StoreResult, SynapseFilter, SynapseRecord,
};
use crate::signal::{NodeId, SignalId, SignalType};
use crate::synapse::SynapseId;

/// One recorded influence attempt.
struct Influence {
    peer: NodeId,
    magnitude: f32,
    at_ns: u64,
}

/// In-memory implementation of [`NodeStore`].
#[derive(Default)]
pub struct MemoryStore {
    synapses: RwLock<HashMap<SynapseId, SynapseRecord>>,
    peers: RwLock<HashMap<NodeId, PeerRecord>>,
    /// Signal identifier to expiry timestamp in nanoseconds.
    seen: Mutex<HashMap<SignalId, u64>>,
    activation: RwLock<Option<ActivationSnapshot>>,
    journal: RwLock<Vec<JournalEntry>>,
    next_journal_id: Mutex<u64>,
    influence: RwLock<Vec<Influence>>,
    meta: RwLock<HashMap<String, Vec<u8>>>,
    history: RwLock<HashMap<SignalId, Vec<u8>>>,
    retain_history: bool,
}

impl MemoryStore {
    /// Create an empty store that does not retain signal bodies.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty store that retains signal bodies for replay.
    #[must_use]
    pub fn with_history() -> Self {
        Self {
            retain_history: true,
            ..Self::default()
        }
    }

    /// Number of live deduplication entries, for tests and metrics.
    ///
    /// # Panics
    /// Panics if a previous caller panicked while holding the lock.
    #[must_use]
    pub fn seen_len(&self) -> usize {
        self.seen.lock().expect("dedup lock poisoned").len()
    }

    /// Number of journal entries, for tests and metrics.
    ///
    /// # Panics
    /// Panics if a previous caller panicked while holding the lock.
    #[must_use]
    pub fn journal_len(&self) -> usize {
        self.journal.read().expect("journal lock poisoned").len()
    }
}

/// Convert a poisoned lock into a store error rather than panicking on a
/// path the runtime can recover from.
fn poisoned(what: &str) -> StoreError {
    StoreError::Unavailable(format!("{what} lock poisoned"))
}

impl NodeStore for MemoryStore {
    fn migrate(&self) -> StoreResult<()> {
        Ok(())
    }

    fn durability(&self) -> Durability {
        Durability::Memory
    }

    fn flush(&self) -> StoreResult<()> {
        Ok(())
    }

    // -- synapses ----------------------------------------------------------

    fn put_synapse(&self, record: &SynapseRecord) -> StoreResult<()> {
        self.synapses
            .write()
            .map_err(|_| poisoned("synapse"))?
            .insert(record.id.clone(), record.clone());
        Ok(())
    }

    fn get_synapse(&self, id: &SynapseId) -> StoreResult<Option<SynapseRecord>> {
        Ok(self
            .synapses
            .read()
            .map_err(|_| poisoned("synapse"))?
            .get(id)
            .cloned())
    }

    fn synapse_for_peer(&self, peer: &NodeId) -> StoreResult<Option<SynapseRecord>> {
        Ok(self
            .synapses
            .read()
            .map_err(|_| poisoned("synapse"))?
            .values()
            .find(|s| &s.peer == peer)
            .cloned())
    }

    fn list_synapses(&self, filter: &SynapseFilter) -> StoreResult<Vec<SynapseRecord>> {
        let guard = self.synapses.read().map_err(|_| poisoned("synapse"))?;
        let mut out: Vec<SynapseRecord> = guard
            .values()
            .filter(|s| filter.states.is_empty() || filter.states.contains(&s.state))
            .filter(|s| filter.min_weight.is_none_or(|min| s.weight >= min))
            .filter(|s| {
                filter
                    .last_active_before_ns
                    .is_none_or(|before| s.last_active_ns < before)
            })
            .cloned()
            .collect();

        // Weight descending, then identifier for a total order so results are
        // reproducible when weights tie.
        out.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });

        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    fn delete_synapse(&self, id: &SynapseId) -> StoreResult<()> {
        self.synapses
            .write()
            .map_err(|_| poisoned("synapse"))?
            .remove(id);
        Ok(())
    }

    // -- topology ----------------------------------------------------------

    fn put_peer(&self, record: &PeerRecord) -> StoreResult<()> {
        self.peers
            .write()
            .map_err(|_| poisoned("peer"))?
            .insert(record.id.clone(), record.clone());
        Ok(())
    }

    fn get_peer(&self, id: &NodeId) -> StoreResult<Option<PeerRecord>> {
        Ok(self
            .peers
            .read()
            .map_err(|_| poisoned("peer"))?
            .get(id)
            .cloned())
    }

    fn list_peers(&self, region: Option<&str>, limit: usize) -> StoreResult<Vec<PeerRecord>> {
        let guard = self.peers.read().map_err(|_| poisoned("peer"))?;
        let mut out: Vec<PeerRecord> = guard
            .values()
            .filter(|p| match region {
                Some(want) => p.region.as_deref() == Some(want),
                None => true,
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            b.last_seen_ns
                .cmp(&a.last_seen_ns)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        out.truncate(limit);
        Ok(out)
    }

    fn count_peers(&self, source: PeerSource) -> StoreResult<u64> {
        Ok(self
            .peers
            .read()
            .map_err(|_| poisoned("peer"))?
            .values()
            .filter(|p| p.source == source)
            .count() as u64)
    }

    // -- deduplication -----------------------------------------------------

    fn check_and_set_seen(&self, id: &SignalId, now_ns: u64, ttl_secs: u64) -> StoreResult<bool> {
        let mut guard = self.seen.lock().map_err(|_| poisoned("dedup"))?;
        let expires = now_ns.saturating_add(ttl_secs.saturating_mul(1_000_000_000));

        match guard.get(id) {
            // A live entry means we have genuinely seen this signal.
            Some(&exp) if exp > now_ns => Ok(true),
            // An expired entry is as good as absent; refresh it in place.
            _ => {
                guard.insert(*id, expires);
                Ok(false)
            }
        }
    }

    fn has_seen(&self, id: &SignalId, now_ns: u64) -> StoreResult<bool> {
        Ok(self
            .seen
            .lock()
            .map_err(|_| poisoned("dedup"))?
            .get(id)
            .is_some_and(|&exp| exp > now_ns))
    }

    fn purge_expired_seen(&self, now_ns: u64) -> StoreResult<u64> {
        let mut guard = self.seen.lock().map_err(|_| poisoned("dedup"))?;
        let before = guard.len();
        guard.retain(|_, &mut exp| exp > now_ns);
        Ok((before - guard.len()) as u64)
    }

    // -- activation --------------------------------------------------------

    fn save_activation(&self, snapshot: &ActivationSnapshot) -> StoreResult<()> {
        *self
            .activation
            .write()
            .map_err(|_| poisoned("activation"))? = Some(*snapshot);
        Ok(())
    }

    fn load_activation(&self) -> StoreResult<Option<ActivationSnapshot>> {
        Ok(*self.activation.read().map_err(|_| poisoned("activation"))?)
    }

    // -- learning journal --------------------------------------------------

    fn append_decision(&self, entry: &JournalEntry) -> StoreResult<JournalId> {
        let id = {
            let mut next = self
                .next_journal_id
                .lock()
                .map_err(|_| poisoned("journal"))?;
            *next += 1;
            JournalId(*next)
        };
        let mut stored = entry.clone();
        stored.id = Some(id);
        self.journal
            .write()
            .map_err(|_| poisoned("journal"))?
            .push(stored);
        Ok(id)
    }

    fn resolve_decision(
        &self,
        id: JournalId,
        outcome: Outcome,
        now_ns: u64,
    ) -> StoreResult<Option<JournalEntry>> {
        let mut guard = self.journal.write().map_err(|_| poisoned("journal"))?;
        let Some(entry) = guard.iter_mut().find(|e| e.id == Some(id)) else {
            return Ok(None);
        };

        // First receipt wins — this is what makes receipt handling idempotent.
        if entry.outcome.is_resolved() {
            return Ok(Some(entry.clone()));
        }

        entry.outcome = outcome;
        entry.resolved_at_ns = Some(now_ns);
        Ok(Some(entry.clone()))
    }

    fn pending_decision_for(
        &self,
        signal: &SignalId,
        peer: &NodeId,
    ) -> StoreResult<Option<JournalEntry>> {
        Ok(self
            .journal
            .read()
            .map_err(|_| poisoned("journal"))?
            .iter()
            .find(|e| &e.signal == signal && &e.peer == peer && !e.outcome.is_resolved())
            .cloned())
    }

    fn expired_decisions(&self, deadline_ns: u64, limit: usize) -> StoreResult<Vec<JournalEntry>> {
        Ok(self
            .journal
            .read()
            .map_err(|_| poisoned("journal"))?
            .iter()
            .filter(|e| !e.outcome.is_resolved() && e.decided_at_ns < deadline_ns)
            .take(limit)
            .cloned()
            .collect())
    }

    fn recent_decisions(
        &self,
        signal_type: Option<&SignalType>,
        limit: usize,
    ) -> StoreResult<Vec<JournalEntry>> {
        let guard = self.journal.read().map_err(|_| poisoned("journal"))?;
        let mut out: Vec<JournalEntry> = guard
            .iter()
            .filter(|e| signal_type.is_none_or(|want| &e.signal_type == want))
            .cloned()
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.decided_at_ns));
        out.truncate(limit);
        Ok(out)
    }

    fn trim_journal(&self, older_than_ns: u64) -> StoreResult<u64> {
        let mut guard = self.journal.write().map_err(|_| poisoned("journal"))?;
        let before = guard.len();
        guard.retain(|e| e.decided_at_ns >= older_than_ns);
        Ok((before - guard.len()) as u64)
    }

    // -- influence accounting ---------------------------------------------

    fn record_influence(
        &self,
        peer: &NodeId,
        magnitude: f32,
        now_ns: u64,
        window_start_ns: u64,
    ) -> StoreResult<f32> {
        let mut guard = self.influence.write().map_err(|_| poisoned("influence"))?;
        guard.push(Influence {
            peer: peer.clone(),
            magnitude: magnitude.abs(),
            at_ns: now_ns,
        });
        Ok(guard
            .iter()
            .filter(|i| &i.peer == peer && i.at_ns >= window_start_ns)
            .map(|i| i.magnitude)
            .sum())
    }

    fn influence_since(&self, peer: &NodeId, window_start_ns: u64) -> StoreResult<f32> {
        Ok(self
            .influence
            .read()
            .map_err(|_| poisoned("influence"))?
            .iter()
            .filter(|i| &i.peer == peer && i.at_ns >= window_start_ns)
            .map(|i| i.magnitude)
            .sum())
    }

    fn purge_influence(&self, older_than_ns: u64) -> StoreResult<u64> {
        let mut guard = self.influence.write().map_err(|_| poisoned("influence"))?;
        let before = guard.len();
        guard.retain(|i| i.at_ns >= older_than_ns);
        Ok((before - guard.len()) as u64)
    }

    // -- node metadata -----------------------------------------------------

    fn put_meta(&self, key: &str, value: &[u8]) -> StoreResult<()> {
        self.meta
            .write()
            .map_err(|_| poisoned("meta"))?
            .insert(key.to_string(), value.to_vec());
        Ok(())
    }

    fn get_meta(&self, key: &str) -> StoreResult<Option<Vec<u8>>> {
        Ok(self
            .meta
            .read()
            .map_err(|_| poisoned("meta"))?
            .get(key)
            .cloned())
    }

    // -- optional: signal history -----------------------------------------

    fn signal_history_enabled(&self) -> bool {
        self.retain_history
    }

    fn put_signal_history(&self, id: &SignalId, body: &[u8], _now_ns: u64) -> StoreResult<()> {
        if !self.retain_history {
            return Err(StoreError::Unsupported("signal history"));
        }
        self.history
            .write()
            .map_err(|_| poisoned("history"))?
            .insert(*id, body.to_vec());
        Ok(())
    }

    fn get_signal_history(&self, id: &SignalId) -> StoreResult<Option<Vec<u8>>> {
        if !self.retain_history {
            return Err(StoreError::Unsupported("signal history"));
        }
        Ok(self
            .history
            .read()
            .map_err(|_| poisoned("history"))?
            .get(id)
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_conforms() {
        super::super::conformance::run_all(&MemoryStore::new());
    }

    #[test]
    fn memory_store_with_history_conforms() {
        super::super::conformance::run_all(&MemoryStore::with_history());
    }

    #[test]
    fn durability_is_memory() {
        assert_eq!(MemoryStore::new().durability(), Durability::Memory);
    }

    #[test]
    fn history_is_off_by_default() {
        assert!(!MemoryStore::new().signal_history_enabled());
        assert!(MemoryStore::with_history().signal_history_enabled());
    }
}
