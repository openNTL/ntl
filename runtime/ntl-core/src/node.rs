//! The NTL node — where storage, activation, propagation, and learning meet.
//!
//! [`Node`] owns no transport and no async runtime. It decides *what* should
//! happen to a signal and records the consequences; a binary wraps it with
//! actual I/O. That split is what lets the whole decision path be tested
//! without a network.

use std::sync::{Arc, Mutex};

use crate::activation::{ActivationState, QueuedSignal};
use crate::config::NodeConfig;
use crate::delivery::{DeliveryClass, Receipt, RejectReason};
use crate::learning::{self, LearningConfig};
use crate::propagation::{self, Chosen};
use crate::rng::{Rng, SplitMix64};
use crate::signal::{NodeId, Signal, SignalBuilder, SignalId};
use crate::store::{
    JournalEntry, NodeStore, Outcome, PeerRecord, PeerSource, SynapseFilter, SynapseRecord,
};
use crate::synapse::{Synapse, SynapseConfig, SynapseId};
use crate::time::Clock;

/// What a node decided to do with a signal.
#[derive(Debug, Clone)]
pub struct Disposition {
    /// Peers the signal should be forwarded to, with the journal entry that
    /// records each decision.
    pub forward_to: Vec<Forward>,
    /// Signals the activation gate released for local handling.
    pub handle_locally: Vec<QueuedSignal>,
    /// A receipt the caller must emit, if the signal was refused and its
    /// class requires one.
    pub receipt: Option<Receipt>,
    /// Why the signal was refused, if it was.
    pub rejected: Option<RejectReason>,
    /// A *different* signal this arrival displaced from the activation queue.
    ///
    /// Separate from [`Self::receipt`] because it is owed to a different
    /// sender than the arriving signal's: overflow is not an exemption from
    /// the delivery guarantee, and the caller cannot infer who to tell from
    /// the arrival alone.
    pub evicted: Option<Evicted>,
}

/// A queued signal displaced by a later arrival.
#[derive(Debug, Clone)]
pub struct Evicted {
    /// The signal that was dropped.
    pub signal: crate::activation::QueuedSignal,
    /// Why it was dropped.
    pub reason: RejectReason,
}

impl Evicted {
    /// Whether the displaced signal's sender must be told.
    #[must_use]
    pub fn needs_receipt(&self) -> bool {
        self.signal.delivery.requires_receipt()
    }
}

impl Disposition {
    /// An empty disposition — nothing to do.
    #[must_use]
    fn nothing() -> Self {
        Self {
            forward_to: Vec::new(),
            handle_locally: Vec::new(),
            receipt: None,
            rejected: None,
            evicted: None,
        }
    }

    /// A refusal, with a receipt if the class demands one.
    fn refuse(signal: &Signal, reason: RejectReason, hops: u16) -> Self {
        Self {
            forward_to: Vec::new(),
            handle_locally: Vec::new(),
            receipt: signal
                .requires_receipt()
                .then(|| Receipt::rejected(signal.id, reason, hops)),
            rejected: Some(reason),
            evicted: None,
        }
    }

    /// Whether the signal was refused.
    #[must_use]
    pub fn was_rejected(&self) -> bool {
        self.rejected.is_some()
    }
}

/// One forwarding decision.
#[derive(Debug, Clone)]
pub struct Forward {
    /// The synapse to send over.
    pub synapse: SynapseId,
    /// The peer on the far end.
    pub peer: NodeId,
    /// The journal entry recording this decision, so its outcome can be
    /// attributed when a receipt arrives.
    pub journal_id: crate::store::JournalId,
    /// Whether this was an exploratory choice.
    pub explored: bool,
}

/// An NTL node.
pub struct Node {
    identity: NodeId,
    config: NodeConfig,
    store: Arc<dyn NodeStore>,
    clock: Arc<dyn Clock>,
    rng: Mutex<Box<dyn Rng>>,
    activation: Mutex<ActivationState>,
}

impl Node {
    /// Start building a node.
    #[must_use]
    pub fn builder() -> NodeBuilder {
        NodeBuilder::new()
    }

    /// This node's identity.
    #[must_use]
    pub fn identity(&self) -> &NodeId {
        &self.identity
    }

    /// This node's configuration.
    #[must_use]
    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// The backing store.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn NodeStore> {
        &self.store
    }

    /// Current time according to this node's clock.
    #[must_use]
    pub fn now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    fn learning(&self) -> &LearningConfig {
        &self.config.learning
    }

    fn synapse_config(&self) -> &SynapseConfig {
        &self.config.synapse
    }

    /// Build and locally register a signal for emission.
    ///
    /// The signal is stamped with this node's identity and marked seen, so a
    /// node never processes its own emission as an arrival.
    ///
    /// # Errors
    /// Returns an error if the signal is invalid or the store rejects the
    /// write.
    pub fn emit(&self, builder: SignalBuilder) -> crate::Result<Signal> {
        let now = self.clock.now_ns();
        let signal = {
            let mut rng = self.rng.lock().map_err(|_| crate::Error::Shutdown)?;
            builder.build_unsigned_with(self.identity.clone(), self.clock.as_ref(), rng.as_mut())
        };

        // Claim our own signal in the dedup cache: without this, a signal that
        // loops back to us looks new.
        self.store
            .check_and_set_seen(&signal.id, now, self.config.propagation.dedup_cache_seconds)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        Ok(signal)
    }

    /// Plan forwarding for a signal this node emitted itself.
    ///
    /// Distinct from [`Self::receive`] because [`Self::emit`] already claimed
    /// the signal in the dedup cache — running it back through `receive`
    /// would see its own claim and drop it. A local emission also skips
    /// admission control: the node chose to send this, so throttling itself
    /// on its own traffic would be backwards.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn receive_local(&self, signal: &Signal) -> crate::Result<Disposition> {
        let now = self.clock.now_ns();
        #[allow(clippy::cast_possible_truncation)]
        let hops = signal.trace.len() as u16;

        if let Err(reason) =
            propagation::check_propagable(signal, &self.identity, &self.config.propagation)
        {
            return Ok(Disposition::refuse(signal, reason, hops));
        }

        let forward_to = self.plan_forwarding(signal, None, now)?;
        if forward_to.is_empty() && signal.requires_receipt() {
            return Ok(Disposition::refuse(signal, RejectReason::NoRoute, hops));
        }

        Ok(Disposition {
            forward_to,
            handle_locally: Vec::new(),
            receipt: None,
            rejected: None,
            evicted: None,
        })
    }

    /// Decide what to do with an arriving signal.
    ///
    /// Applies the propagation rules in cheapest-first order — size and TTL,
    /// then deduplication, then routing — so an attacker cannot impose
    /// expensive work with malformed traffic.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn receive(
        &self,
        signal: &Signal,
        arrival_synapse: Option<&SynapseId>,
    ) -> crate::Result<Disposition> {
        let now = self.clock.now_ns();
        #[allow(clippy::cast_possible_truncation)]
        let hops = signal.trace.len() as u16;

        // Rule 4: deduplication, before any routing work.
        let seen = self
            .store
            .check_and_set_seen(&signal.id, now, self.config.propagation.dedup_cache_seconds)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        if seen {
            return Ok(Disposition::nothing());
        }

        // Rules 1-3, including the acknowledged-delivery exception to silent
        // absorption.
        if let Err(reason) =
            propagation::check_propagable(signal, &self.identity, &self.config.propagation)
        {
            return Ok(Disposition::refuse(signal, reason, hops));
        }

        // Admission control.
        let synapse_weight = arrival_synapse
            .and_then(|id| self.store.get_synapse(id).ok().flatten())
            .map_or(1.0, |r| r.weight);

        let queued = QueuedSignal {
            id: signal.id,
            origin: signal.origin.clone(),
            weight: signal.weight,
            delivery: signal.delivery,
            enqueued_at_ns: now,
        };

        let admit = {
            let mut rng = self.rng.lock().map_err(|_| crate::Error::Shutdown)?;
            let mut gate = self.activation.lock().map_err(|_| crate::Error::Shutdown)?;
            gate.admit(queued, synapse_weight, now, rng.as_mut())
        };

        // A dropped acknowledged signal owes its sender a receipt: overload is
        // not an exemption from the delivery guarantee.
        let mut evicted = None;
        if let Some(dropped) = &admit.dropped {
            let reason = admit.drop_reason.unwrap_or(RejectReason::QueueFull);
            if dropped.id == signal.id {
                return Ok(Disposition::refuse(signal, reason, hops));
            }
            // A different signal was displaced to make room for this one. Its
            // sender is owed the same receipt the arriving signal would have
            // been owed, and it is a different peer — which is why this is
            // reported rather than folded into `receipt`. Reporting it is what
            // `AdmitOutcome`'s documented guarantee requires, and dropping it
            // silently was the one overflow policy that broke that promise.
            evicted = Some(Evicted {
                signal: dropped.clone(),
                reason,
            });
        }

        if admit.fired.is_empty() {
            // Queued, not refused. Nothing to do yet beyond any eviction.
            return Ok(Disposition {
                evicted,
                ..Disposition::nothing()
            });
        }

        // Route onward.
        let forward_to = self.plan_forwarding(signal, arrival_synapse, now)?;

        if forward_to.is_empty() && signal.requires_receipt() {
            // Nowhere to send it and the sender must be told.
            return Ok(Disposition {
                evicted,
                ..Disposition::refuse(signal, RejectReason::NoRoute, hops)
            });
        }

        Ok(Disposition {
            forward_to,
            handle_locally: admit.fired,
            receipt: None,
            rejected: None,
            evicted,
        })
    }

    /// Plan forwarding for a signal the activation gate has already released.
    ///
    /// The gate is a queue, so a signal can be admitted on one arrival and
    /// released later — in a batch alongside a different arrival, or by the
    /// latency guard. At that point it still needs a route, and
    /// [`Self::receive`] cannot have planned one because it did not know the
    /// signal would fire.
    ///
    /// Deduplication and admission are deliberately not re-run: this signal
    /// has already passed both, and `check_and_set_seen` would now see its own
    /// claim and report it as a duplicate.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn plan_release(
        &self,
        signal: &Signal,
        arrival_synapse: Option<&SynapseId>,
    ) -> crate::Result<Vec<Forward>> {
        let now = self.clock.now_ns();
        self.plan_forwarding(signal, arrival_synapse, now)
    }

    /// Record that a forwarding decision could not be transmitted.
    ///
    /// A journalled decision whose peer turned out to be unreachable must be
    /// resolved, not left pending: the learning model treats silence as a
    /// pending decision, so an untransmitted forward would sit in the journal
    /// until the timeout sweep attributed it to the *path* rather than to the
    /// transport. Resolving it immediately teaches the model the same thing
    /// several seconds sooner.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn fail_forward(
        &self,
        journal_id: crate::store::JournalId,
    ) -> crate::Result<Option<learning::WeightUpdate>> {
        let now = self.clock.now_ns();
        let resolved = self
            .store
            .resolve_decision(journal_id, Outcome::TransportFailure, now)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        // First outcome wins, so a decision a receipt already resolved is
        // untouched here.
        let Some(entry) = resolved else {
            return Ok(None);
        };
        if entry.resolved_at_ns != Some(now) {
            return Ok(None);
        }
        self.apply_outcome_to_synapse(&entry, Outcome::TransportFailure, now)
            .map(Some)
    }

    /// Choose synapses for a signal and journal each decision.
    fn plan_forwarding(
        &self,
        signal: &Signal,
        arrival_synapse: Option<&SynapseId>,
        now_ns: u64,
    ) -> crate::Result<Vec<Forward>> {
        let records = self
            .store
            .list_synapses(&SynapseFilter::eligible())
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        if records.is_empty() {
            return Ok(Vec::new());
        }

        // Rehydrate with decay applied: a weight that has not been touched in
        // a week should not route as though it were fresh.
        let synapses: Vec<Synapse> = records
            .iter()
            .map(|r| {
                let mut s = Synapse::from_record(r, self.identity.clone(), self.synapse_config());
                s.weight = s.decayed_weight(now_ns, self.learning());
                s
            })
            .collect();

        let type_name = signal_type_name(signal);
        let chosen: Vec<Chosen<'_>> = {
            let mut rng = self.rng.lock().map_err(|_| crate::Error::Shutdown)?;
            propagation::select_synapses(
                &synapses,
                &signal.scope,
                &type_name,
                arrival_synapse.map(|s| s.0.as_str()),
                &self.config.propagation,
                self.learning(),
                now_ns,
                rng.as_mut(),
            )
        };

        let mut out = Vec::with_capacity(chosen.len());
        for c in chosen {
            let entry = JournalEntry {
                id: None,
                signal: signal.id,
                signal_type: signal.signal_type.clone(),
                synapse: c.synapse.id.clone(),
                peer: c.synapse.remote_node.clone(),
                score: c.score,
                signal_weight: signal.weight,
                explored: c.explored,
                decided_at_ns: now_ns,
                outcome: Outcome::Pending,
                resolved_at_ns: None,
            };
            let journal_id = self
                .store
                .append_decision(&entry)
                .map_err(|e| crate::Error::Serialization(e.to_string()))?;

            out.push(Forward {
                synapse: c.synapse.id.clone(),
                peer: c.synapse.remote_node.clone(),
                journal_id,
                explored: c.explored,
            });
        }
        Ok(out)
    }

    /// Apply a receipt: resolve its decision and update the synapse weight.
    ///
    /// Returns the weight update applied, or `None` if the receipt matched no
    /// pending decision — an unmatched receipt is discarded, not an error,
    /// since forged receipts would otherwise be the cheapest attack on the
    /// routing model.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn apply_receipt(
        &self,
        receipt: &Receipt,
        from_peer: &NodeId,
    ) -> crate::Result<Option<learning::WeightUpdate>> {
        let now = self.clock.now_ns();

        let Some(pending) = self
            .store
            .pending_decision_for(&receipt.correlation_id, from_peer)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(journal_id) = pending.id else {
            return Ok(None);
        };

        let resolved = self
            .store
            .resolve_decision(journal_id, receipt.outcome(), now)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        // resolve_decision keeps the first outcome, so a replayed receipt has
        // no cumulative effect.
        let Some(entry) = resolved else {
            return Ok(None);
        };
        if entry.resolved_at_ns != Some(now) {
            return Ok(None);
        }

        self.apply_outcome_to_synapse(&entry, receipt.outcome(), now)
            .map(Some)
    }

    /// Convert decisions past their deadline into `TimedOut` and learn from
    /// them.
    ///
    /// This is what makes failure observable. A path that never delivers
    /// produces no receipt, so without this sweep it looks identical to a path
    /// never tried, and the model can never learn to avoid it.
    ///
    /// Returns how many decisions were resolved.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn sweep_timeouts(&self, limit: usize) -> crate::Result<usize> {
        let now = self.clock.now_ns();
        let deadline = self.learning().receipt_deadline(now);

        let expired = self
            .store
            .expired_decisions(deadline, limit)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        let mut resolved = 0;
        for entry in expired {
            let Some(id) = entry.id else { continue };
            let updated = self
                .store
                .resolve_decision(id, Outcome::TimedOut, now)
                .map_err(|e| crate::Error::Serialization(e.to_string()))?;
            if let Some(e) = updated {
                if e.resolved_at_ns == Some(now) {
                    self.apply_outcome_to_synapse(&e, Outcome::TimedOut, now)?;
                    resolved += 1;
                }
            }
        }
        Ok(resolved)
    }

    /// Apply one resolved outcome to its synapse, respecting the influence
    /// cap and renormalising afterwards.
    fn apply_outcome_to_synapse(
        &self,
        entry: &JournalEntry,
        outcome: Outcome,
        now_ns: u64,
    ) -> crate::Result<learning::WeightUpdate> {
        let learning_cfg = self.learning();
        let window_start = learning_cfg.influence_window_start(now_ns);

        let influence_used = self
            .store
            .influence_since(&entry.peer, window_start)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        let Some(record) = self
            .store
            .get_synapse(&entry.synapse)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?
        else {
            // The synapse was pruned while the decision was in flight.
            return Ok(learning::WeightUpdate {
                before: 0.0,
                after: 0.0,
                applied_delta: 0.0,
                capped: false,
            });
        };

        let mut synapse =
            Synapse::from_record(&record, self.identity.clone(), self.synapse_config());
        let type_name = signal_type_name_of(&entry.signal_type);

        let update = synapse.apply_outcome(
            outcome,
            entry.signal_weight.clamp(0.0, 1.0),
            &type_name,
            influence_used,
            learning_cfg,
        );

        if update.applied_delta.abs() > f32::EPSILON {
            self.store
                .record_influence(&entry.peer, update.applied_delta, now_ns, window_start)
                .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        }

        synapse.last_active_ns = now_ns;
        self.store
            .put_synapse(&synapse.to_record())
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        self.normalize_outbound_weights()?;
        Ok(update)
    }

    /// Rescale outbound weights if they exceed the node's total budget.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn normalize_outbound_weights(&self) -> crate::Result<Option<f32>> {
        let records = self
            .store
            .list_synapses(&SynapseFilter::default())
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        if records.is_empty() {
            return Ok(None);
        }

        let mut weights: Vec<f32> = records.iter().map(|r| r.weight).collect();
        let Some(factor) = learning::normalize_outbound(&mut weights, self.learning()) else {
            return Ok(None);
        };

        for (record, weight) in records.iter().zip(&weights) {
            let mut updated = record.clone();
            updated.weight = *weight;
            self.store
                .put_synapse(&updated)
                .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        }
        Ok(Some(factor))
    }

    /// Penalise a synapse whose peer presented an unverifiable signature.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn penalize_signature_failure(&self, synapse_id: &SynapseId) -> crate::Result<()> {
        let Some(record) = self
            .store
            .get_synapse(synapse_id)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?
        else {
            return Ok(());
        };
        let mut synapse =
            Synapse::from_record(&record, self.identity.clone(), self.synapse_config());
        synapse.apply_signature_failure(self.learning());
        self.store
            .put_synapse(&synapse.to_record())
            .map_err(|e| crate::Error::Serialization(e.to_string()))
    }

    /// Register or update a synapse to a peer.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn upsert_synapse(&self, peer: &NodeId) -> crate::Result<SynapseRecord> {
        let now = self.clock.now_ns();
        if let Some(existing) = self
            .store
            .synapse_for_peer(peer)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?
        {
            return Ok(existing);
        }

        let synapse = {
            let mut rng = self.rng.lock().map_err(|_| crate::Error::Shutdown)?;
            let mut s = Synapse::new_with(
                self.identity.clone(),
                peer.clone(),
                self.synapse_config(),
                now,
                rng.as_mut(),
            );
            // A synapse that stays Forming can never carry traffic. The
            // handshake is the transport's job, and this method is only
            // reached once it has verified the peer's identity against the
            // key that signed it — so completing it here is correct.
            s.activate();
            s
        };

        let record = synapse.to_record();
        self.store
            .put_synapse(&record)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        self.store
            .put_peer(&PeerRecord {
                id: peer.clone(),
                addresses: Vec::new(),
                region: None,
                advertised_types: Vec::new(),
                last_seen_ns: now,
                source: PeerSource::Observed,
            })
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        Ok(record)
    }

    /// Release any signals the activation queue has held past its latency
    /// guard.
    ///
    /// Backpressure must be a delay, not an indefinite hold. The guard inside
    /// `receive` can only act when another signal arrives, so a node whose
    /// traffic goes quiet needs this called periodically — otherwise a
    /// below-threshold signal waits forever and its sender times out on a
    /// path that was working.
    ///
    /// # Errors
    /// Returns an error if the activation lock is poisoned.
    pub fn poll_activation(&self) -> crate::Result<Vec<QueuedSignal>> {
        let now = self.clock.now_ns();
        let mut gate = self.activation.lock().map_err(|_| crate::Error::Shutdown)?;
        Ok(gate.drain_if_starving(now))
    }

    /// Persist the activation snapshot.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn checkpoint(&self) -> crate::Result<()> {
        let now = self.clock.now_ns();
        let snapshot = {
            let gate = self.activation.lock().map_err(|_| crate::Error::Shutdown)?;
            gate.snapshot(now)
        };
        self.store
            .save_activation(&snapshot)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        self.store
            .purge_expired_seen(now)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        self.store
            .flush()
            .map_err(|e| crate::Error::Serialization(e.to_string()))
    }

    /// Health of the routing model, for observability.
    ///
    /// The two ratios are the model's health check: exploration near zero
    /// means the node has stopped learning; a pending ratio near one means it
    /// is not receiving the receipts it needs, and its weights reflect
    /// nothing.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable.
    pub fn learning_health(&self, sample: usize) -> crate::Result<LearningHealth> {
        let recent = self
            .store
            .recent_decisions(None, sample)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;

        let total = recent.len();
        if total == 0 {
            return Ok(LearningHealth::default());
        }
        let explored = recent.iter().filter(|e| e.explored).count();
        let pending = recent.iter().filter(|e| !e.outcome.is_resolved()).count();
        let delivered = recent
            .iter()
            .filter(|e| e.outcome == Outcome::Delivered)
            .count();

        #[allow(clippy::cast_precision_loss)]
        let denom = total as f32;
        #[allow(clippy::cast_precision_loss)]
        let health = LearningHealth {
            decisions_sampled: total,
            exploration_ratio: explored as f32 / denom,
            pending_ratio: pending as f32 / denom,
            delivery_ratio: delivered as f32 / denom,
        };
        Ok(health)
    }
}

/// Observability summary for the routing model.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LearningHealth {
    /// How many recent decisions were examined.
    pub decisions_sampled: usize,
    /// Fraction reached by exploration. Near zero means learning has stopped.
    pub exploration_ratio: f32,
    /// Fraction still awaiting an outcome. Near one means no receipts are
    /// arriving, so the weights reflect nothing.
    pub pending_ratio: f32,
    /// Fraction that delivered successfully.
    pub delivery_ratio: f32,
}

/// The signal type name used as the affinity key.
fn signal_type_name(signal: &Signal) -> String {
    signal_type_name_of(&signal.signal_type)
}

fn signal_type_name_of(t: &crate::signal::SignalType) -> String {
    match t {
        crate::signal::SignalType::Custom(name) => name.clone(),
        other => format!("{other:?}"),
    }
}

/// Builder for [`Node`].
pub struct NodeBuilder {
    config: Option<NodeConfig>,
    config_path: Option<String>,
    identity: Option<NodeId>,
    store: Option<Arc<dyn NodeStore>>,
    clock: Option<Arc<dyn Clock>>,
    rng: Option<Box<dyn Rng>>,
}

impl Default for NodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeBuilder {
    /// A builder with nothing configured.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: None,
            config_path: None,
            identity: None,
            store: None,
            clock: None,
            rng: None,
        }
    }

    /// Supply configuration directly.
    #[must_use]
    pub fn with_config(mut self, config: NodeConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Load configuration from a TOML file.
    #[must_use]
    pub fn with_config_file(mut self, path: &str) -> Self {
        self.config_path = Some(path.to_string());
        self
    }

    /// Set this node's identity. Defaults to a placeholder derived from the
    /// store, so a node without crypto still has a stable id.
    #[must_use]
    pub fn with_identity(mut self, identity: NodeId) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Set the storage backend.
    #[must_use]
    pub fn with_store(mut self, store: Arc<dyn NodeStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Set the clock. Defaults to the host clock where one exists.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Set the randomness source. Defaults to one seeded from identity and
    /// time.
    #[must_use]
    pub fn with_rng(mut self, rng: Box<dyn Rng>) -> Self {
        self.rng = Some(rng);
        self
    }

    /// Build the node.
    ///
    /// Runs schema migrations, validates configuration, and restores any
    /// persisted activation state.
    ///
    /// # Errors
    /// Returns [`crate::Error::Config`] if configuration is invalid or no
    /// store was supplied, or a storage error if migration fails.
    pub fn build(self) -> crate::Result<Node> {
        let config = match (self.config, self.config_path) {
            (Some(c), _) => c,
            (None, Some(path)) => NodeConfig::from_file(&path)?,
            (None, None) => NodeConfig::default(),
        };
        config.validate().map_err(crate::Error::Config)?;

        let store = self
            .store
            .ok_or_else(|| crate::Error::Config("no storage backend configured".to_string()))?;
        store
            .migrate()
            .map_err(|e| crate::Error::Config(format!("migration failed: {e}")))?;

        let clock: Arc<dyn Clock> = match self.clock {
            Some(c) => c,
            #[cfg(not(target_arch = "wasm32"))]
            None => Arc::new(crate::time::SystemClock),
            #[cfg(target_arch = "wasm32")]
            None => {
                return Err(crate::Error::Config(
                    "a clock must be supplied on this target".to_string(),
                ));
            }
        };

        let now = clock.now_ns();

        // Identity persists across restarts, so a node keeps the synapses it
        // has already formed.
        let identity = match self.identity {
            Some(id) => {
                store
                    .put_meta("node-id", &id.0)
                    .map_err(|e| crate::Error::Config(e.to_string()))?;
                id
            }
            None => {
                if let Some(bytes) = store
                    .get_meta("node-id")
                    .map_err(|e| crate::Error::Config(e.to_string()))?
                {
                    NodeId(bytes)
                } else {
                    let mut seed = SplitMix64::seeded(now);
                    let mut bytes = Vec::with_capacity(32);
                    for _ in 0..4 {
                        bytes.extend_from_slice(&seed.next_u64().to_le_bytes());
                    }
                    store
                        .put_meta("node-id", &bytes)
                        .map_err(|e| crate::Error::Config(e.to_string()))?;
                    NodeId(bytes)
                }
            }
        };

        let rng = self
            .rng
            .unwrap_or_else(|| Box::new(SplitMix64::from_identity(&identity.0, now)));

        let mut activation = ActivationState::new(&config.activation);
        if let Some(snapshot) = store
            .load_activation()
            .map_err(|e| crate::Error::Config(e.to_string()))?
        {
            // Restoring backpressure matters: discarding it makes a restart a
            // free reset for anyone flooding the node.
            activation.restore(&snapshot);
        }

        Ok(Node {
            identity,
            config,
            store,
            clock,
            rng: Mutex::new(rng),
            activation: Mutex::new(activation),
        })
    }
}

/// Convenience: the delivery class a signal builder should use for a
/// transactional emission.
#[must_use]
pub fn transactional() -> DeliveryClass {
    DeliveryClass::Acknowledged
}

/// Convenience: look up a signal's dedup state without inserting.
///
/// # Errors
/// Returns an error if the store is unavailable.
pub fn has_seen(store: &dyn NodeStore, id: &SignalId, now_ns: u64) -> crate::Result<bool> {
    store
        .has_seen(id, now_ns)
        .map_err(|e| crate::Error::Serialization(e.to_string()))
}
