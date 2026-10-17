#![cfg(test)]

use refract_backstop::{RefractBackstop, RefractBackstopClient};
use refract_bounty::{DataKey as BountyDataKey, RefractBountyEscrow, RefractBountyEscrowClient};
use refract_grants::{GrantsDataKey, RefractGrantsContract, RefractGrantsContractClient};
use refract_treasury::{DataKey as TreasuryDataKey, RefractTreasury, RefractTreasuryClient};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events as _},
    token,
    xdr::{ScVal, ToXdr},
    Address, BytesN, Env, IntoVal, TryFromVal, Val,
};
use std::fs;
use std::path::Path;

/// Convert any Soroban Val into its canonical XDR byte representation formatted as lowercase hex.
fn val_to_xdr_hex(env: &Env, val: Val) -> String {
    let scval = ScVal::try_from_val(env, &val).expect("Val to ScVal conversion");
    let bytes = scval.to_xdr(env);
    let mut buf = vec![0u8; bytes.len() as usize];
    bytes.copy_into_slice(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compute a line-by-line diff between expected and actual text snapshots.
fn generate_diff(expected: &str, actual: &str) -> String {
    let exp_lines: Vec<&str> = expected.lines().collect();
    let act_lines: Vec<&str> = actual.lines().collect();
    let mut diff = String::new();
    let max_len = exp_lines.len().max(act_lines.len());
    for i in 0..max_len {
        let exp = exp_lines.get(i).copied().unwrap_or("<EOF>");
        let act = act_lines.get(i).copied().unwrap_or("<EOF>");
        if exp != act {
            diff.push_str(&format!("Line {}:\n  - {}\n  + {}\n", i + 1, exp, act));
        }
    }
    if diff.is_empty() && expected != actual {
        diff.push_str("Content differs in trailing newlines or whitespace\n");
    }
    diff
}

/// Verification helper: returns Ok(()) if snapshot matches, or Err(diff) if it diverges.
fn verify_snapshot(expected: &str, actual: &str) -> Result<(), String> {
    if expected == actual {
        Ok(())
    } else {
        Err(generate_diff(expected, actual))
    }
}

fn golden_file_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("golden")
        .join(format!("{}.golden", name))
}

/// Compares actual snapshot with golden file on disk.
/// If REGENERATE_GOLDENS=1, overwrites/creates the golden file.
/// Otherwise asserts equality and fails with a readable diff on mismatch.
fn assert_or_regenerate_golden(contract_name: &str, actual: &str) {
    let path = golden_file_path(contract_name);
    let regenerate = std::env::var("REGENERATE_GOLDENS")
        .map(|v| v == "1")
        .unwrap_or(false);
    if regenerate {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&path, actual).expect("failed to write golden file");
        println!("Golden snapshot regenerated at {:?}", path);
    } else {
        if !path.exists() {
            panic!(
                "Golden file does not exist at {:?}. Run with REGENERATE_GOLDENS=1 cargo test --test snapshot_tests to create it.",
                path
            );
        }
        let expected = fs::read_to_string(&path).expect("failed to read golden file");
        if let Err(diff) = verify_snapshot(&expected, actual) {
            panic!(
                "Golden mismatch for {:?}\nDiff:\n{}\nRun with REGENERATE_GOLDENS=1 cargo test --test snapshot_tests to update golden files.",
                path, diff
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Treasury Contract Snapshot Generator
// ─────────────────────────────────────────────────────────────────────────────
fn generate_treasury_snapshot() -> String {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let timelock = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let recipient = Address::generate(&env);

    client.initialize(&admin);
    client.set_timelock(&admin, &timelock);
    client.set_reserve_target(&admin, &token_id, &100_000_000);

    sac.mint(&admin, &100_000_000);
    client.deposit_reserve(&admin, &token_id, &50_000_000);

    sac.mint(&contract_id, &50_000_000);
    client.withdraw(&admin, &token_id, &10_000_000, &recipient);

    client.release_reserve(
        &timelock,
        &token_id,
        &20_000_000,
        &recipient,
        &symbol_short!("grant"),
    );

    let mut out = String::new();
    out.push_str("# Contract: RefractTreasury\n");
    out.push_str("# Snapshot: Event and Storage Encodings\n\n");

    out.push_str("[EVENTS]\n");
    let mut evt_idx = 0;
    for (cid, topics, data) in env.events().all() {
        if cid == contract_id {
            out.push_str(&format!("event[{}].contract = {:?}\n", evt_idx, cid));
            for (t_idx, topic) in topics.iter().enumerate() {
                out.push_str(&format!(
                    "event[{}].topic[{}] = {:?}\n",
                    evt_idx, t_idx, topic
                ));
                out.push_str(&format!(
                    "event[{}].topic[{}].xdr = {}\n",
                    evt_idx,
                    t_idx,
                    val_to_xdr_hex(&env, topic)
                ));
            }
            out.push_str(&format!("event[{}].data = {:?}\n", evt_idx, data));
            out.push_str(&format!(
                "event[{}].data.xdr = {}\n",
                evt_idx,
                val_to_xdr_hex(&env, data)
            ));
            evt_idx += 1;
        }
    }

    out.push_str("\n[STORAGE]\n");
    env.as_contract(&contract_id, || {
        let keys: Vec<(&'static str, TreasuryDataKey)> = vec![
            ("instance", TreasuryDataKey::Admin),
            ("instance", TreasuryDataKey::TimelockContract),
            (
                "instance",
                TreasuryDataKey::ReserveBalance(token_id.clone()),
            ),
            ("instance", TreasuryDataKey::ReserveTarget(token_id.clone())),
            (
                "instance",
                TreasuryDataKey::OperatingBalance(token_id.clone()),
            ),
            ("instance", TreasuryDataKey::LastRelease(token_id.clone())),
        ];
        for (i, (scope, k)) in keys.into_iter().enumerate() {
            let k_val: Val = k.clone().into_val(&env);
            out.push_str(&format!("storage[{}].scope = {}\n", i, scope));
            out.push_str(&format!("storage[{}].key = {:?}\n", i, k));
            out.push_str(&format!(
                "storage[{}].key.xdr = {}\n",
                i,
                val_to_xdr_hex(&env, k_val)
            ));
            let v_val: Val = match &k {
                TreasuryDataKey::Admin => env
                    .storage()
                    .instance()
                    .get::<_, Address>(&k)
                    .unwrap()
                    .into_val(&env),
                TreasuryDataKey::TimelockContract => env
                    .storage()
                    .instance()
                    .get::<_, Address>(&k)
                    .unwrap()
                    .into_val(&env),
                TreasuryDataKey::ReserveBalance(_) => env
                    .storage()
                    .instance()
                    .get::<_, i128>(&k)
                    .unwrap()
                    .into_val(&env),
                TreasuryDataKey::ReserveTarget(_) => env
                    .storage()
                    .instance()
                    .get::<_, i128>(&k)
                    .unwrap()
                    .into_val(&env),
                TreasuryDataKey::OperatingBalance(_) => env
                    .storage()
                    .instance()
                    .get::<_, i128>(&k)
                    .unwrap()
                    .into_val(&env),
                TreasuryDataKey::LastRelease(_) => env
                    .storage()
                    .instance()
                    .get::<_, u64>(&k)
                    .unwrap()
                    .into_val(&env),
                TreasuryDataKey::ChallengeThresholdAmount => env
                    .storage()
                    .instance()
                    .get::<_, i128>(&k)
                    .unwrap_or(0)
                    .into_val(&env),
                TreasuryDataKey::ChallengeWindowSeconds => env
                    .storage()
                    .instance()
                    .get::<_, u64>(&k)
                    .unwrap_or(0)
                    .into_val(&env),
                TreasuryDataKey::NextWithdrawalId => env
                    .storage()
                    .instance()
                    .get::<_, u64>(&k)
                    .unwrap_or(1)
                    .into_val(&env),
                TreasuryDataKey::Withdrawal(_id) => env
                    .storage()
                    .persistent()
                    .get::<_, refract_treasury::WithdrawalRecord>(&k)
                    .expect("withdrawal")
                    .into_val(&env),
                TreasuryDataKey::KeeperDelegation(_keeper, _action) => env
                    .storage()
                    .persistent()
                    .get::<_, refract_treasury::KeeperDelegation>(&k)
                    .expect("keeper delegation")
                    .into_val(&env),
            };
            out.push_str(&format!("storage[{}].val = {:?}\n", i, v_val));
            out.push_str(&format!(
                "storage[{}].val.xdr = {}\n",
                i,
                val_to_xdr_hex(&env, v_val)
            ));
        }
    });

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Bounty Contract Snapshot Generator
// ─────────────────────────────────────────────────────────────────────────────
fn generate_bounty_snapshot() -> String {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let funder = Address::generate(&env);
    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[42u8; 32]);

    client.initialize(&governance, &token_id);

    sac.mint(&funder, &100_000_000);
    client.fund(&funder, &100_000_000);

    client.award(&governance, &researcher, &30_000_000, &report_hash);

    let mut out = String::new();
    out.push_str("# Contract: RefractBountyEscrow\n");
    out.push_str("# Snapshot: Event and Storage Encodings\n\n");

    out.push_str("[EVENTS]\n");
    let mut evt_idx = 0;
    for (cid, topics, data) in env.events().all() {
        if cid == contract_id {
            out.push_str(&format!("event[{}].contract = {:?}\n", evt_idx, cid));
            for (t_idx, topic) in topics.iter().enumerate() {
                out.push_str(&format!(
                    "event[{}].topic[{}] = {:?}\n",
                    evt_idx, t_idx, topic
                ));
                out.push_str(&format!(
                    "event[{}].topic[{}].xdr = {}\n",
                    evt_idx,
                    t_idx,
                    val_to_xdr_hex(&env, topic)
                ));
            }
            out.push_str(&format!("event[{}].data = {:?}\n", evt_idx, data));
            out.push_str(&format!(
                "event[{}].data.xdr = {}\n",
                evt_idx,
                val_to_xdr_hex(&env, data)
            ));
            evt_idx += 1;
        }
    }

    out.push_str("\n[STORAGE]\n");
    env.as_contract(&contract_id, || {
        let instance_keys: Vec<(&'static str, BountyDataKey)> = vec![
            ("instance", BountyDataKey::Governance),
            ("instance", BountyDataKey::Token),
            ("instance", BountyDataKey::Balance),
        ];
        let mut idx = 0;
        for (scope, k) in instance_keys {
            let k_val: Val = k.clone().into_val(&env);
            out.push_str(&format!("storage[{}].scope = {}\n", idx, scope));
            out.push_str(&format!("storage[{}].key = {:?}\n", idx, k));
            out.push_str(&format!(
                "storage[{}].key.xdr = {}\n",
                idx,
                val_to_xdr_hex(&env, k_val)
            ));
            let v_val: Val = match &k {
                BountyDataKey::Governance => env
                    .storage()
                    .instance()
                    .get::<_, Address>(&k)
                    .unwrap()
                    .into_val(&env),
                BountyDataKey::Token => env
                    .storage()
                    .instance()
                    .get::<_, Address>(&k)
                    .unwrap()
                    .into_val(&env),
                BountyDataKey::Balance => env
                    .storage()
                    .instance()
                    .get::<_, i128>(&k)
                    .unwrap()
                    .into_val(&env),
                _ => unreachable!(),
            };
            out.push_str(&format!("storage[{}].val = {:?}\n", idx, v_val));
            out.push_str(&format!(
                "storage[{}].val.xdr = {}\n",
                idx,
                val_to_xdr_hex(&env, v_val)
            ));
            idx += 1;
        }

        let p_key = BountyDataKey::Award(report_hash.clone());
        let p_key_val: Val = p_key.clone().into_val(&env);
        out.push_str(&format!("storage[{}].scope = persistent\n", idx));
        out.push_str(&format!("storage[{}].key = {:?}\n", idx, p_key));
        out.push_str(&format!(
            "storage[{}].key.xdr = {}\n",
            idx,
            val_to_xdr_hex(&env, p_key_val)
        ));
        let award_rec = env
            .storage()
            .persistent()
            .get::<_, refract_bounty::AwardRecord>(&p_key)
            .unwrap();
        let rec_val: Val = award_rec.into_val(&env);
        out.push_str(&format!("storage[{}].val = {:?}\n", idx, rec_val));
        out.push_str(&format!(
            "storage[{}].val.xdr = {}\n",
            idx,
            val_to_xdr_hex(&env, rec_val)
        ));
    });

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Backstop Contract Snapshot Generator
// ─────────────────────────────────────────────────────────────────────────────
fn generate_backstop_snapshot() -> String {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &contract_id);

    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let staker = Address::generate(&env);

    client.initialize(&pool, &admin, &token_id, &1000);

    sac.mint(&staker, &100_000_000);
    client.deposit(&staker, &100_000_000);

    client.cover_shortfall(&pool, &25_000_000);
    client.withdraw(&staker, &10_000_000);

    let mut out = String::new();
    out.push_str("# Contract: RefractBackstop\n");
    out.push_str("# Snapshot: Event and Storage Encodings\n\n");

    out.push_str("[EVENTS]\n");
    let mut evt_idx = 0;
    for (cid, topics, data) in env.events().all() {
        if cid == contract_id {
            out.push_str(&format!("event[{}].contract = {:?}\n", evt_idx, cid));
            for (t_idx, topic) in topics.iter().enumerate() {
                out.push_str(&format!(
                    "event[{}].topic[{}] = {:?}\n",
                    evt_idx, t_idx, topic
                ));
                out.push_str(&format!(
                    "event[{}].topic[{}].xdr = {}\n",
                    evt_idx,
                    t_idx,
                    val_to_xdr_hex(&env, topic)
                ));
            }
            out.push_str(&format!("event[{}].data = {:?}\n", evt_idx, data));
            out.push_str(&format!(
                "event[{}].data.xdr = {}\n",
                evt_idx,
                val_to_xdr_hex(&env, data)
            ));
            evt_idx += 1;
        }
    }

    out.push_str("\n[STORAGE]\n");
    env.as_contract(&contract_id, || {
        let config_key = symbol_short!("config");
        let config_val: Val = config_key.into_val(&env);
        let config_obj = env
            .storage()
            .instance()
            .get::<_, refract_backstop::BackstopConfig>(&"config")
            .unwrap();
        let cfg_val: Val = config_obj.into_val(&env);
        out.push_str("storage[0].scope = instance\n");
        out.push_str(&format!("storage[0].key = {:?}\n", config_key));
        out.push_str(&format!(
            "storage[0].key.xdr = {}\n",
            val_to_xdr_hex(&env, config_val)
        ));
        out.push_str(&format!("storage[0].val = {:?}\n", cfg_val));
        out.push_str(&format!(
            "storage[0].val.xdr = {}\n",
            val_to_xdr_hex(&env, cfg_val)
        ));

        let state_key = symbol_short!("state");
        let state_key_val: Val = state_key.into_val(&env);
        let state_obj = env
            .storage()
            .instance()
            .get::<_, refract_backstop::BackstopState>(&"state")
            .unwrap();
        let st_val: Val = state_obj.into_val(&env);
        out.push_str("storage[1].scope = instance\n");
        out.push_str(&format!("storage[1].key = {:?}\n", state_key));
        out.push_str(&format!(
            "storage[1].key.xdr = {}\n",
            val_to_xdr_hex(&env, state_key_val)
        ));
        out.push_str(&format!("storage[1].val = {:?}\n", st_val));
        out.push_str(&format!(
            "storage[1].val.xdr = {}\n",
            val_to_xdr_hex(&env, st_val)
        ));

        let shares_key = ("shares", staker.clone());
        let shares_key_val: Val = shares_key.clone().into_val(&env);
        let shares_amt: i128 = env.storage().persistent().get(&shares_key).unwrap();
        let sh_val: Val = shares_amt.into_val(&env);
        out.push_str("storage[2].scope = persistent\n");
        out.push_str(&format!("storage[2].key = {:?}\n", shares_key));
        out.push_str(&format!(
            "storage[2].key.xdr = {}\n",
            val_to_xdr_hex(&env, shares_key_val)
        ));
        out.push_str(&format!("storage[2].val = {:?}\n", sh_val));
        out.push_str(&format!(
            "storage[2].val.xdr = {}\n",
            val_to_xdr_hex(&env, sh_val)
        ));
    });

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Grants Contract Snapshot Generator
// ─────────────────────────────────────────────────────────────────────────────
fn generate_grants_snapshot() -> String {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, RefractGrantsContract);
    let client = RefractGrantsContractClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let recipient = Address::generate(&env);

    client.initialize(&governance);

    let mut milestones = soroban_sdk::Vec::new(&env);
    milestones.push_back(10_000_000);
    milestones.push_back(20_000_000);

    let grant_id = client.create_grant(&governance, &recipient, &30_000_000, &milestones);
    client.confirm_milestone(&governance, &grant_id, &0);
    client.claim(&recipient, &grant_id);

    let mut out = String::new();
    out.push_str("# Contract: RefractGrants\n");
    out.push_str("# Snapshot: Event and Storage Encodings\n\n");

    out.push_str("[EVENTS]\n");
    let mut evt_idx = 0;
    for (cid, topics, data) in env.events().all() {
        if cid == contract_id {
            out.push_str(&format!("event[{}].contract = {:?}\n", evt_idx, cid));
            for (t_idx, topic) in topics.iter().enumerate() {
                out.push_str(&format!(
                    "event[{}].topic[{}] = {:?}\n",
                    evt_idx, t_idx, topic
                ));
                out.push_str(&format!(
                    "event[{}].topic[{}].xdr = {}\n",
                    evt_idx,
                    t_idx,
                    val_to_xdr_hex(&env, topic)
                ));
            }
            out.push_str(&format!("event[{}].data = {:?}\n", evt_idx, data));
            out.push_str(&format!(
                "event[{}].data.xdr = {}\n",
                evt_idx,
                val_to_xdr_hex(&env, data)
            ));
            evt_idx += 1;
        }
    }

    out.push_str("\n[STORAGE]\n");
    env.as_contract(&contract_id, || {
        let k_gov = GrantsDataKey::Governance;
        let k_gov_val: Val = k_gov.clone().into_val(&env);
        let gov_addr: Address = env.storage().instance().get(&k_gov).unwrap();
        let gov_val: Val = gov_addr.into_val(&env);
        out.push_str("storage[0].scope = instance\n");
        out.push_str(&format!("storage[0].key = {:?}\n", k_gov));
        out.push_str(&format!(
            "storage[0].key.xdr = {}\n",
            val_to_xdr_hex(&env, k_gov_val)
        ));
        out.push_str(&format!("storage[0].val = {:?}\n", gov_val));
        out.push_str(&format!(
            "storage[0].val.xdr = {}\n",
            val_to_xdr_hex(&env, gov_val)
        ));

        let k_next = GrantsDataKey::NextGrantId;
        let k_next_val: Val = k_next.clone().into_val(&env);
        let next_id: u64 = env.storage().instance().get(&k_next).unwrap();
        let next_val: Val = next_id.into_val(&env);
        out.push_str("storage[1].scope = instance\n");
        out.push_str(&format!("storage[1].key = {:?}\n", k_next));
        out.push_str(&format!(
            "storage[1].key.xdr = {}\n",
            val_to_xdr_hex(&env, k_next_val)
        ));
        out.push_str(&format!("storage[1].val = {:?}\n", next_val));
        out.push_str(&format!(
            "storage[1].val.xdr = {}\n",
            val_to_xdr_hex(&env, next_val)
        ));

        let k_grant = GrantsDataKey::Grant(grant_id);
        let k_grant_val: Val = k_grant.clone().into_val(&env);
        let grant_rec = env
            .storage()
            .persistent()
            .get::<_, refract_grants::GrantRecord>(&k_grant)
            .unwrap();
        let grant_val: Val = grant_rec.into_val(&env);
        out.push_str("storage[2].scope = persistent\n");
        out.push_str(&format!("storage[2].key = {:?}\n", k_grant));
        out.push_str(&format!(
            "storage[2].key.xdr = {}\n",
            val_to_xdr_hex(&env, k_grant_val)
        ));
        out.push_str(&format!("storage[2].val = {:?}\n", grant_val));
        out.push_str(&format!(
            "storage[2].val.xdr = {}\n",
            val_to_xdr_hex(&env, grant_val)
        ));
    });

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Golden Snapshot Integration Tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_treasury_snapshot_encodings() {
    let snapshot = generate_treasury_snapshot();
    assert_or_regenerate_golden("treasury", &snapshot);
}

#[test]
fn test_bounty_snapshot_encodings() {
    let snapshot = generate_bounty_snapshot();
    assert_or_regenerate_golden("bounty", &snapshot);
}

#[test]
fn test_backstop_snapshot_encodings() {
    let snapshot = generate_backstop_snapshot();
    assert_or_regenerate_golden("backstop", &snapshot);
}

#[test]
fn test_grants_snapshot_encodings() {
    let snapshot = generate_grants_snapshot();
    assert_or_regenerate_golden("grants", &snapshot);
}

// ─────────────────────────────────────────────────────────────────────────────
// Meta-tests: Verify that any mutation in event topics, data, or XDR causes failure
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_meta_intentional_event_mutation_fails() {
    let golden = generate_treasury_snapshot();
    let mut mutated = golden.clone();
    mutated = mutated.replace("Symbol(treasury)", "Symbol(treasury_tampered)");
    assert_ne!(golden, mutated);
    let result = verify_snapshot(&golden, &mutated);
    assert!(result.is_err(), "Mutated snapshot must fail verification");
    let err = result.unwrap_err();
    assert!(
        err.contains("treasury_tampered"),
        "Diff must indicate the mutated token"
    );
}

#[test]
fn test_meta_intentional_xdr_byte_mutation_fails() {
    let golden = generate_bounty_snapshot();
    // Tamper with one hex character in XDR encoding
    let mut mutated = golden.clone();
    mutated = mutated.replacen(".xdr = 00", ".xdr = ff", 1);
    assert_ne!(golden, mutated);
    let result = verify_snapshot(&golden, &mutated);
    assert!(result.is_err(), "Mutated XDR byte must fail verification");
    let diff = result.unwrap_err();
    assert!(
        diff.contains(".xdr = ff"),
        "Diff must pinpoint mutated XDR hex line"
    );
}

#[test]
fn test_meta_golden_files_exist_and_match() {
    for name in &["treasury", "bounty", "backstop", "grants"] {
        let path = golden_file_path(name);
        assert!(path.exists(), "Golden file {:?} must exist on disk", path);
        let content = fs::read_to_string(&path).expect("read golden file");
        assert!(
            !content.trim().is_empty(),
            "Golden file {:?} must not be empty",
            path
        );
    }
}
