#![cfg(test)]

//! Resource measurements behind MAX_EXPIRE_BATCH, the chunked index and
//! the numbers quoted in the PR. The measuring helper mirrors
//! policy/src/bench.rs. `measure` reads the test host's recorded
//! footprint around one call, so entry and byte counts are what the
//! network would charge for (read bytes: each footprint entry's size before
//! the call; write bytes: each read-write entry's size after it). CPU is
//! the host's metered instruction count; the contract runs natively here,
//! so it excludes Wasm execution and is best read comparatively.
//!
//! Print the tables with:
//! `cargo test -p refract-pool bench -- --nocapture --test-threads 1`

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

use refract_policy::{RefractPolicyRegistry, RefractPolicyRegistryClient};
use soroban_sdk::{testutils::Ledger as _, token::StellarAssetClient};

const ONE_USDC: i128 = 10_000_000;

struct Bench<'a> {
    env: Env,
    pool: RefractPoolClient<'a>,
    registry: RefractPolicyRegistryClient<'a>,
    usdc_admin: StellarAssetClient<'a>,
    admin: Address,
}

fn setup<'a>() -> Bench<'a> {
    let env = Env::new_with_config(soroban_sdk::testutils::EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    env.mock_all_auths();
    env.budget().reset_unlimited();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc_admin = StellarAssetClient::new(&env, &sac.address());
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);
    let registry_id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &registry_id);
    registry.initialize(&admin, &pool_id);
    pool.initialize(&admin, &sac.address(), &registry_id);

    let lp = Address::generate(&env);
    usdc_admin.mint(&lp, &(100_000_000 * ONE_USDC));
    pool.provide_capital(&lp, &(100_000_000 * ONE_USDC));
    Bench {
        env,
        pool,
        registry,
        usdc_admin,
        admin,
    }
}

fn holder(b: &Bench) -> Address {
    let h = Address::generate(&b.env);
    b.usdc_admin.mint(&h, &(1_000_000 * ONE_USDC));
    h
}

fn params() -> PolicyParams {
    PolicyParams {
        coverage_amount: 100 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    }
}

fn row(label: &str, c: &Cost) {
    println!(
        "| {label} | {} | {} | {} | {} | {} | {} |",
        c.read_entries, c.read_bytes, c.write_entries, c.write_bytes, c.event_bytes, c.cpu
    );
}

fn header(title: &str) {
    println!(
        "\n{title}\n| case | read entries | read bytes | write entries | write bytes | event bytes | CPU insns |"
    );
    println!("|---|---|---|---|---|---|---|");
}

fn add(a: &mut Cost, c: &Cost) {
    a.cpu += c.cpu;
    a.read_entries += c.read_entries;
    a.write_entries += c.write_entries;
    a.read_bytes += c.read_bytes;
    a.write_bytes += c.write_bytes;
    a.event_bytes += c.event_bytes;
}

/// #59: a single holder's 1st, 50th and 500th buy_policy, which appends to
/// both the pool's and the registry's per-holder index. "settled"
/// deactivates each registry record before the next purchase, isolating
/// the lifetime indexes; "all active" keeps every one live, so the
/// registry's O(active) active index grows too.
#[test]
fn bench_purchase_by_history_length() {
    header("buy_policy, Nth purchase by one holder");
    for settle in [true, false] {
        let b = setup();
        let h = holder(&b);
        for n in 1..=500u32 {
            let (id, c) = measure(&b.env, || b.pool.buy_policy(&h, &params()));
            if [1, 50, 500].contains(&n) {
                let mode = if settle { "settled" } else { "all active" };
                row(&std::format!("#{n}, {mode}"), &c);
            }
            if settle {
                b.registry.deactivate_policy(&b.admin, &id);
            }
        }
        // Control: see policy/src/bench.rs — host CPU in the test env grows
        // with the whole ledger, not with this holder's history.
        if settle {
            let fresh = holder(&b);
            let (_, c) = measure(&b.env, || b.pool.buy_policy(&fresh, &params()));
            row("control: #1 for a fresh holder, same ledger", &c);
        }
    }
}

/// #56: N expire_policy calls (summed) vs one expire_policies(N). Every
/// policy has its own holder — the worst case for the registry's
/// per-holder active index. Also checks a batch at MAX_EXPIRE_BATCH fits
/// the live per-transaction limits.
#[test]
fn bench_expire_batch() {
    header("expire: N singles (summed) vs one batch");
    for n in [1u32, 10, MAX_EXPIRE_BATCH] {
        let b = setup();
        let mut single_ids = StdVec::new();
        let mut batch_ids = soroban_sdk::Vec::new(&b.env);
        for i in 0..2 * n {
            let id = b.pool.buy_policy(&holder(&b), &params());
            if i < n {
                single_ids.push(id);
            } else {
                batch_ids.push_back(id);
            }
        }
        b.env.ledger().with_mut(|li| li.timestamp += 31 * 86_400);

        let mut singles = Cost {
            cpu: 0,
            read_entries: 0,
            write_entries: 0,
            read_bytes: 0,
            write_bytes: 0,
            event_bytes: 0,
        };
        for id in single_ids {
            let (_, c) = measure_with_events(&b.env, || b.pool.expire_policy(&id));
            add(&mut singles, &c);
        }
        let (_, batch) = measure_with_events(&b.env, || b.pool.expire_policies(&batch_ids));
        row(&std::format!("{n} × expire_policy"), &singles);
        row(&std::format!("expire_policies({n})"), &batch);

        if n == MAX_EXPIRE_BATCH {
            assert!(batch.read_entries <= 400, "{batch:?}");
            assert!(batch.write_entries <= 200, "{batch:?}");
            assert!(batch.write_bytes <= 132_096, "{batch:?}");
            assert!(batch.event_bytes <= 16_384, "{batch:?}");
        }
    }
}
