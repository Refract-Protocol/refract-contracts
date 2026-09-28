#![cfg(test)]

//! Resource measurements behind the numbers quoted in the index-related
//! constants and in the PR. `measure` reads the test host's recorded
//! footprint around one call, so entry and byte counts are what the
//! network would charge for (read bytes: each footprint entry's size before
//! the call; write bytes: each read-write entry's size after it). CPU is
//! the host's metered instruction count; the contract runs natively here,
//! so it excludes Wasm execution and is best read comparatively.
//!
//! Print the tables with:
//! `cargo test -p refract-policy bench -- --nocapture --test-threads 1`

extern crate std;

use super::*;
use soroban_env_host::{
    storage::AccessType,
    xdr::{ContractEventType, LedgerKey, Limits, WriteXdr},
};
use soroban_sdk::testutils::Address as _;
use std::{println, rc::Rc, vec::Vec as StdVec};

#[derive(Debug, Clone, Copy)]
pub struct Cost {
    pub cpu: u64,
    pub read_entries: u32,
    pub write_entries: u32,
    pub read_bytes: u32,
    pub write_bytes: u32,
    pub event_bytes: u32,
}

/// Run `f` (one contract invocation) and report what it cost, leaving
/// `event_bytes` at 0.
pub fn measure<R>(env: &Env, f: impl FnOnce() -> R) -> (R, Cost) {
    measure_inner(env, false, f)
}

/// `measure`, also sizing the contract events `f` emitted. Separate because
/// reading the host's event log copies the whole log, which dominates the
/// runtime of tests that measure hundreds of calls.
pub fn measure_with_events<R>(env: &Env, f: impl FnOnce() -> R) -> (R, Cost) {
    measure_inner(env, true, f)
}

fn measure_inner<R>(env: &Env, with_events: bool, f: impl FnOnce() -> R) -> (R, Cost) {
    let host = env.host();
    let budget = host.budget_cloned();
    let before = host
        .with_mut_storage(|s| {
            s.footprint = Default::default();
            Ok(s.map.clone())
        })
        .unwrap();
    let events_before = if with_events {
        host.get_events().unwrap().0.len()
    } else {
        0
    };
    env.budget().reset_unlimited();

    let r = f();

    let cpu = env.budget().cpu_instruction_cost();
    let size = |e: Option<&Option<soroban_env_host::storage::EntryWithLiveUntil>>| {
        e.and_then(|e| e.as_ref()).map_or(0, |(entry, _)| {
            entry.to_xdr(Limits::none()).unwrap().len() as u32
        })
    };
    let mut cost = host
        .with_mut_storage(|s| {
            let mut c = Cost {
                cpu,
                read_entries: 0,
                write_entries: 0,
                read_bytes: 0,
                write_bytes: 0,
                event_bytes: 0,
            };
            for (key, access) in s.footprint.0.iter(&budget)? {
                c.read_entries += 1;
                c.read_bytes += size(before.get::<Rc<LedgerKey>>(key, &budget)?);
                if *access == AccessType::ReadWrite {
                    c.write_entries += 1;
                    c.write_bytes += size(s.map.get::<Rc<LedgerKey>>(key, &budget)?);
                }
            }
            Ok(c)
        })
        .unwrap();

    if with_events {
        for e in host.get_events().unwrap().0.iter().skip(events_before) {
            if !e.failed_call && e.event.type_ == ContractEventType::Contract {
                cost.event_bytes += e.event.to_xdr(Limits::none()).unwrap().len() as u32;
            }
        }
    }
    (r, cost)
}

fn setup<'a>() -> (Env, RefractPolicyRegistryClient<'a>, Address) {
    let env = Env::new_with_config(soroban_sdk::testutils::EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    env.mock_all_auths();
    // Several tests register hundreds of policies in one Env, which would
    // exhaust the default per-Env budget.
    env.budget().reset_unlimited();
    let admin = Address::generate(&env);
    let pool = Address::generate(&env);
    let id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &id);
    registry.initialize(&admin, &pool);
    (env, registry, pool)
}

fn reg(id: u64, holder: &Address) -> PolicyRegistration {
    PolicyRegistration {
        policy_id: id,
        holder: holder.clone(),
        coverage_type: CoverageType::StablecoinDepeg,
        coverage_amount: 100_000_000,
        premium: 1_000_000,
        expires_at: 9_999_999_999,
    }
}

fn row(label: &str, c: &Cost) {
    println!(
        "| {label} | {} | {} | {} | {} | {} |",
        c.read_entries, c.read_bytes, c.write_entries, c.write_bytes, c.cpu
    );
}

fn header(title: &str) {
    println!(
        "\n{title}\n| case | read entries | read bytes | write entries | write bytes | CPU insns |"
    );
    println!("|---|---|---|---|---|---|");
}

/// Size of a chunk entry as a function of how many ids it holds — the
/// input to the INDEX_CHUNK_SIZE derivation.
#[test]
fn bench_chunk_entry_size() {
    let (env, registry, pool) = setup();
    let holder = Address::generate(&env);
    let mut sizes = StdVec::new();
    for id in 0..INDEX_CHUNK_SIZE as u64 {
        registry.register_policy(&pool, &reg(id, &holder));
        if id == 0 || id == INDEX_CHUNK_SIZE as u64 - 1 {
            let bytes = env.as_contract(&registry.address, || {
                let host = env.host();
                let budget = host.budget_cloned();
                host.with_mut_storage(|s| {
                    let mut total = 0;
                    for (_, v) in s.map.iter(&budget)? {
                        if let Some((entry, _)) = v {
                            let xdr = entry.to_xdr(Limits::none()).unwrap();
                            // The chunk is the only entry whose key names
                            // HolderChunk.
                            if std::format!("{:?}", entry).contains("HolderChunk") {
                                total = xdr.len();
                            }
                        }
                    }
                    Ok(total)
                })
                .unwrap()
            });
            sizes.push((id + 1, bytes));
        }
    }
    let (n1, s1) = sizes[0];
    let (n2, s2) = sizes[1];
    let per_id = (s2 - s1) / (n2 - n1) as usize;
    let fixed = s1 - per_id;
    println!("\nchunk entry: {fixed} bytes fixed + {per_id} bytes/id; full chunk = {s2} bytes");
    assert_eq!(per_id, 12);
    // The derivation: a full chunk's byte fee stays under one entry fee.
    assert!(s2 as u64 * 875 / 1024 < 2_500, "{s2}");
    assert!(
        (s2 + 128 * 12) as u64 * 875 / 1024 >= 2_500,
        "256-id chunks would also fit"
    );
}

/// #58: get_holder_active_policy_ids for a holder with L lifetime policies
/// and 3 active. The "legacy" rows are the same holder before
/// rebuild_active_index — i.e. the pre-change full scan.
#[test]
fn bench_active_ids_query() {
    header("get_holder_active_policy_ids (3 active)");
    for lifetime in [10u64, 100, 500] {
        let (env, registry, pool) = setup();
        let holder = Address::generate(&env);
        for id in 0..lifetime {
            registry.register_policy(&pool, &reg(id, &holder));
        }
        for id in 0..lifetime - 3 {
            registry.deactivate_policy(&pool, &id);
        }
        let (_, indexed) = measure(&env, || registry.get_holder_active_policy_ids(&holder));
        row(&std::format!("{lifetime} lifetime, indexed"), &indexed);
        assert_eq!(indexed.read_entries, 2);

        if lifetime <= MAX_SCAN_BATCH as u64 {
            env.as_contract(&registry.address, || {
                env.storage()
                    .persistent()
                    .remove(&DataKey::HolderActivePolicies(holder.clone()));
            });
            let (_, scan) = measure(&env, || registry.get_holder_active_policy_ids(&holder));
            row(&std::format!("{lifetime} lifetime, full scan"), &scan);
        }
    }
}

/// #58: what maintaining the active index adds to register/deactivate, for
/// a holder with A policies already active.
#[test]
fn bench_active_index_maintenance() {
    header("register / deactivate with A already active");
    for active in [0u64, 10, 100] {
        let (env, registry, pool) = setup();
        let holder = Address::generate(&env);
        for id in 0..active {
            registry.register_policy(&pool, &reg(id, &holder));
        }
        let (_, r) = measure(&env, || {
            registry.register_policy(&pool, &reg(active, &holder))
        });
        row(&std::format!("register, A={active}"), &r);
        let (_, d) = measure(&env, || registry.deactivate_policy(&pool, &active));
        row(&std::format!("deactivate, A={active}"), &d);
    }
}

/// #59 (registry half): cost of a holder's 1st, 50th and 500th
/// registration. "settled" deactivates each policy before the next one is
/// registered, isolating the lifetime-history index; "all active" keeps
/// every one live, so the O(active) active index grows too (the worst case
/// for #58's write-side cost).
#[test]
fn bench_register_by_history_length() {
    header("register_policy, Nth for one holder");
    for settle in [true, false] {
        let (env, registry, pool) = setup();
        let holder = Address::generate(&env);
        for id in 0..500u64 {
            let (_, c) = measure(&env, || registry.register_policy(&pool, &reg(id, &holder)));
            if [0, 49, 499].contains(&id) {
                let mode = if settle { "settled" } else { "all active" };
                row(&std::format!("#{}, {mode}", id + 1), &c);
            }
            if settle {
                registry.deactivate_policy(&pool, &id);
            }
        }
        // Control: a brand-new holder's first registration on the same
        // (now 500-policy) ledger. The test host's storage map holds every
        // entry the test has created and its metered inserts copy it, so
        // host CPU grows with the ledger regardless of this holder's
        // history; on the network a transaction's map holds only its
        // footprint. Byte and entry counts are unaffected.
        if settle {
            let fresh = Address::generate(&env);
            let (_, c) = measure(&env, || {
                registry.register_policy(&pool, &reg(10_000, &fresh))
            });
            row("control: #1 for a fresh holder, same ledger", &c);
        }
    }
}

/// #57: one deactivate_policies call vs N deactivate_policy calls. Each
/// holder is distinct (the worst case: one active-index entry per id).
/// Also checks that a batch at MAX_DEACTIVATE_BATCH fits the live limits.
#[test]
fn bench_deactivate_batch() {
    header("deactivate: N singles (summed) vs one batch");
    for n in [1u32, 10, MAX_DEACTIVATE_BATCH] {
        let (env, registry, pool) = setup();
        let mut ids = soroban_sdk::Vec::new(&env);
        for id in 0..(2 * n) as u64 {
            registry.register_policy(&pool, &reg(id, &Address::generate(&env)));
            if id < n as u64 {
                ids.push_back(id);
            }
        }
        let mut singles = Cost {
            cpu: 0,
            read_entries: 0,
            write_entries: 0,
            read_bytes: 0,
            write_bytes: 0,
            event_bytes: 0,
        };
        for id in n as u64..(2 * n) as u64 {
            let (_, c) = measure(&env, || registry.deactivate_policy(&pool, &id));
            singles.cpu += c.cpu;
            singles.read_entries += c.read_entries;
            singles.write_entries += c.write_entries;
            singles.read_bytes += c.read_bytes;
            singles.write_bytes += c.write_bytes;
        }
        let (_, batch) = measure_with_events(&env, || registry.deactivate_policies(&pool, &ids));
        row(&std::format!("{n} × deactivate_policy"), &singles);
        row(&std::format!("deactivate_policies({n})"), &batch);

        if n == MAX_DEACTIVATE_BATCH {
            assert!(batch.read_entries <= 400, "{batch:?}");
            assert!(batch.write_entries <= 200, "{batch:?}");
            assert!(batch.write_bytes <= 132_096, "{batch:?}");
            assert!(batch.event_bytes <= 16_384, "{batch:?}");
        }
    }
}
