//! Benchmarks for the operations on a signal's hot path.
//!
//! What matters here is the *shape* of the numbers, not their absolute value.
//! NTL's claim is that a routing decision is cheap enough to make per signal
//! on modest hardware, so path scoring and selection are the figures to watch:
//! if they are not microseconds, the claim is in trouble.

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use ntl_core::NodeStore;
use ntl_core::learning::{ExplorationPolicy, LearningConfig};
use ntl_core::propagation::{
    PropagationConfig, PropagationScope, ScoringWeights, score_synapse, select_synapses,
};
use ntl_core::rng::SplitMix64;
use ntl_core::signal::{NodeId, Signal};
use ntl_core::store::{MemoryStore, Outcome};
use ntl_core::synapse::{Synapse, SynapseConfig, SynapseState};

const NOW: u64 = 1_700_000_000 * 1_000_000_000;

fn origin() -> NodeId {
    NodeId(vec![0u8; 32])
}

fn topology(n: u8) -> Vec<Synapse> {
    (0..n)
        .map(|i| {
            let mut rng = SplitMix64::seeded(u64::from(i));
            let mut s = Synapse::new_with(
                origin(),
                NodeId(vec![i + 1; 32]),
                &SynapseConfig::default(),
                NOW,
                &mut rng,
            );
            s.weight = 0.1 + f32::from(i % 9) * 0.1;
            s.state = SynapseState::Active;
            s.last_active_ns = NOW;
            s.type_affinity.insert("Data".into(), u64::from(i));
            s
        })
        .collect()
}

fn signal_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("signal");

    group.bench_function("build_unsigned", |b| {
        b.iter(|| {
            Signal::data("benchmark")
                .with_payload(serde_json::json!({"key": "value"}))
                .with_weight(0.5)
                .build_unsigned(origin())
        });
    });

    let signal = Signal::data("benchmark")
        .with_payload(serde_json::json!({"key": "value", "n": 42}))
        .with_weight(0.5)
        .build_unsigned(origin());

    group.bench_function("encode", |b| {
        b.iter(|| signal.encode().expect("encode"));
    });

    let encoded = signal.encode().expect("encode");
    group.bench_function("decode", |b| {
        b.iter(|| Signal::decode(black_box(&encoded)).expect("decode"));
    });

    group.bench_function("signing_bytes", |b| {
        b.iter(|| signal.signing_bytes().expect("signing bytes"));
    });

    group.bench_function("validate", |b| {
        let mut valid = signal.clone();
        valid.signature = vec![0u8; 64];
        b.iter(|| valid.validate());
    });

    group.finish();
}

fn path_scoring(c: &mut Criterion) {
    let mut group = c.benchmark_group("scoring");
    let weights = ScoringWeights::default();
    let synapses = topology(64);

    group.bench_function("score_one_synapse", |b| {
        b.iter(|| score_synapse(black_box(&synapses[0]), "Data", NOW, &weights));
    });

    // The number that carries the "cheap enough per signal" claim.
    for size in [8usize, 64, 256] {
        let topo = topology(u8::try_from(size.min(255)).expect("fits"));
        let config = PropagationConfig::default();
        let learning = LearningConfig::default();

        group.bench_function(format!("select_softmax_{size}_synapses"), |b| {
            b.iter_batched(
                || SplitMix64::seeded(1),
                |mut rng| {
                    select_synapses(
                        &topo,
                        &PropagationScope::default(),
                        "Data",
                        None,
                        &config,
                        &learning,
                        NOW,
                        &mut rng,
                    )
                },
                BatchSize::SmallInput,
            );
        });

        let greedy = PropagationConfig {
            exploration: ExplorationPolicy::EpsilonGreedy { epsilon: 0.1 },
            ..PropagationConfig::default()
        };
        group.bench_function(format!("select_epsilon_greedy_{size}_synapses"), |b| {
            b.iter_batched(
                || SplitMix64::seeded(1),
                |mut rng| {
                    select_synapses(
                        &topo,
                        &PropagationScope::default(),
                        "Data",
                        None,
                        &greedy,
                        &learning,
                        NOW,
                        &mut rng,
                    )
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn learning_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("learning");
    let config = LearningConfig::default();

    group.bench_function("apply_reward", |b| {
        b.iter(|| {
            ntl_core::learning::apply_reward(black_box(0.5), Outcome::Delivered, 0.8, 0.0, &config)
        });
    });

    group.bench_function("decayed_weight", |b| {
        b.iter(|| {
            ntl_core::learning::decayed_weight(
                black_box(0.5),
                NOW,
                NOW + 48 * ntl_core::time::NANOS_PER_HOUR,
                &config,
            )
        });
    });

    group.bench_function("normalize_outbound_64", |b| {
        b.iter_batched(
            || vec![0.5f32; 64],
            |mut weights| ntl_core::learning::normalize_outbound(&mut weights, &config),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn activation_decision(c: &mut Criterion) {
    use ntl_core::activation::{ActivationConfig, ActivationState, QueuedSignal};
    use ntl_core::delivery::DeliveryClass;

    let mut group = c.benchmark_group("activation");

    group.bench_function("admit", |b| {
        b.iter_batched(
            || {
                (
                    ActivationState::new(&ActivationConfig::default()),
                    SplitMix64::seeded(1),
                )
            },
            |(mut state, mut rng)| {
                for i in 0..16u64 {
                    state.admit(
                        QueuedSignal {
                            id: ntl_core::SignalId::from_parts(
                                1_700_000_000_000 + i,
                                u128::from(i),
                            ),
                            origin: origin(),
                            weight: 0.5,
                            delivery: DeliveryClass::BestEffort,
                            enqueued_at_ns: NOW,
                        },
                        0.5,
                        NOW,
                        &mut rng,
                    );
                }
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn store_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("store");
    let store = MemoryStore::new();
    store.migrate().expect("migrate");

    for s in topology(64) {
        store.put_synapse(&s.to_record()).expect("put");
    }

    group.bench_function("list_eligible_synapses_64", |b| {
        b.iter(|| {
            store
                .list_synapses(&ntl_core::store::SynapseFilter::eligible())
                .expect("list")
        });
    });

    // Deduplication is on the path of every single arriving signal, so its
    // cost is paid more often than anything else here.
    group.bench_function("dedup_check_and_set", |b| {
        let mut n = 0u64;
        b.iter(|| {
            n += 1;
            store
                .check_and_set_seen(
                    &ntl_core::SignalId::from_parts(1_700_000_000_000, u128::from(n)),
                    NOW,
                    300,
                )
                .expect("dedup")
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    signal_encoding,
    path_scoring,
    learning_updates,
    activation_decision,
    store_operations
);
criterion_main!(benches);
