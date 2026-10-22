#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.

extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    vec, String, TryFromVal, Val,
};
use std::vec::Vec as StdVec;

struct Setup {
    env: Env,
    contract_id: Address,
    admin: Address,
    token: Address,
}

impl Setup {
    fn client(&self) -> RefractTreasuryClient<'_> {
        RefractTreasuryClient::new(&self.env, &self.contract_id)
    }
    fn token_client(&self) -> token::Client<'_> {
        token::Client::new(&self.env, &self.token)
    }
    fn sac(&self) -> token::StellarAssetClient<'_> {
        token::StellarAssetClient::new(&self.env, &self.token)
    }
}

fn setup(balance: i128) -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);
    let contract_id = env.register_contract(None, RefractTreasury);
    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let s = Setup {
        env,
        contract_id,
        admin,
        token,
    };
    s.client().initialize(&s.admin);
    if balance > 0 {
        s.sac().mint(&s.contract_id, &balance);
    }
    s
}

fn salt(env: &Env, i: u32) -> BytesN<32> {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&i.to_be_bytes());
    BytesN::from_array(env, &b)
}

/// Mirror of `scripts/merkle_batch.py`: sorted leaves, odd node carried up.
fn build_levels(env: &Env, leaves: &[BytesN<32>]) -> StdVec<StdVec<BytesN<32>>> {
    let mut first: StdVec<BytesN<32>> = leaves.to_vec();
    first.sort_by_key(|a| a.to_array());
    let mut levels = std::vec![first];
    while levels.last().unwrap().len() > 1 {
        let cur = levels.last().unwrap();
        let mut next = StdVec::new();
        let mut i = 0;
        while i + 1 < cur.len() {
            next.push(node_hash(env, &cur[i], &cur[i + 1]));
            i += 2;
        }
        if cur.len() % 2 == 1 {
            next.push(cur[cur.len() - 1].clone());
        }
        levels.push(next);
    }
    levels
}

fn proof_for(env: &Env, levels: &[StdVec<BytesN<32>>], leaf: &BytesN<32>) -> Vec<BytesN<32>> {
    let mut idx = levels[0].iter().position(|l| l == leaf).unwrap();
    let mut proof = Vec::new(env);
    for level in &levels[..levels.len() - 1] {
        let sib = idx ^ 1;
        if sib < level.len() {
            proof.push_back(level[sib].clone());
        }
        idx /= 2;
    }
    proof
}

struct Leaf {
    recipient: Address,
    amount: i128,
    salt: BytesN<32>,
    hash: BytesN<32>,
}

struct Tree {
    leaves: StdVec<Leaf>,
    levels: StdVec<StdVec<BytesN<32>>>,
}

impl Tree {
    fn new(env: &Env, asset: &Address, amounts: &[i128]) -> Tree {
        let leaves: StdVec<Leaf> = amounts
            .iter()
            .enumerate()
            .map(|(i, amount)| {
                let recipient = Address::generate(env);
                let salt = salt(env, i as u32);
                let hash = leaf_hash(env, &recipient, asset, *amount, &salt);
                Leaf {
                    recipient,
                    amount: *amount,
                    salt,
                    hash,
                }
            })
            .collect();
        let hashes: StdVec<BytesN<32>> = leaves.iter().map(|l| l.hash.clone()).collect();
        let levels = build_levels(env, &hashes);
        Tree { leaves, levels }
    }

    fn root(&self) -> BytesN<32> {
        self.levels.last().unwrap()[0].clone()
    }

    fn proof(&self, env: &Env, i: usize) -> Vec<BytesN<32>> {
        proof_for(env, &self.levels, &self.leaves[i].hash)
    }
}

fn claim(s: &Setup, tree: &Tree, batch_id: u64, i: usize) -> Result<i128, TreasuryError> {
    let l = &tree.leaves[i];
    s.client()
        .try_claim_leaf(
            &batch_id,
            &l.recipient,
            &l.amount,
            &l.salt,
            &tree.proof(&s.env, i),
        )
        .map(|r| r.unwrap())
        .map_err(|e| e.unwrap())
}

// ---------------------------------------------------------------------------
// Basic custody
// ---------------------------------------------------------------------------

#[test]
fn initialize_twice_is_rejected() {
    let s = setup(0);
    assert_eq!(
        s.client().try_initialize(&s.admin),
        Err(Ok(TreasuryError::AlreadyInitialized))
    );
}

#[test]
fn withdraw_is_admin_gated() {
    let s = setup(1_000);
    assert_eq!(s.client().admin(), s.admin);
    let to = Address::generate(&s.env);
    s.client().withdraw(&s.admin, &s.token, &400, &to);
    assert_eq!(s.token_client().balance(&to), 400);
    assert_eq!(s.token_client().balance(&s.contract_id), 600);
}

#[test]
fn withdraw_unauthorized_is_rejected() {
    let s = setup(1_000);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client().try_withdraw(&stranger, &s.token, &1, &stranger),
        Err(Ok(TreasuryError::Unauthorized))
    );
}

#[test]
fn withdraw_cannot_touch_reserved_or_dust() {
    let s = setup(1_000);
    let tree = Tree::new(&s.env, &s.token, &[300, 300]);
    s.client()
        .publish_batch(&s.admin, &tree.root(), &600, &s.token, &2_000);
    let donor = Address::generate(&s.env);
    s.sac().mint(&donor, &50);
    s.client().credit_dust(&donor, &s.token, &50);

    assert_eq!(s.client().reserved(&s.token), 600);
    assert_eq!(s.client().dust_balance(&s.token), 50);
    assert_eq!(s.client().free_balance(&s.token), 400);
    let to = Address::generate(&s.env);
    assert_eq!(
        s.client().try_withdraw(&s.admin, &s.token, &401, &to),
        Err(Ok(TreasuryError::InsufficientFreeBalance))
    );
    s.client().withdraw(&s.admin, &s.token, &400, &to);
}

// ---------------------------------------------------------------------------
// Dust account (SPEC.md §8)
// ---------------------------------------------------------------------------

#[test]
fn dust_is_credited_and_withdrawn_separately() {
    let s = setup(0);
    let donor = Address::generate(&s.env);
    s.sac().mint(&donor, &7);
    assert_eq!(s.client().credit_dust(&donor, &s.token, &7), 7);
    assert_eq!(s.client().dust_balance(&s.token), 7);
    assert_eq!(s.client().free_balance(&s.token), 0);

    let to = Address::generate(&s.env);
    assert_eq!(
        s.client().try_withdraw_dust(&s.admin, &s.token, &8, &to),
        Err(Ok(TreasuryError::InsufficientDust))
    );
    s.client().withdraw_dust(&s.admin, &s.token, &7, &to);
    assert_eq!(s.client().dust_balance(&s.token), 0);
    assert_eq!(s.token_client().balance(&to), 7);
}

#[test]
fn withdraw_dust_unauthorized_is_rejected() {
    let s = setup(0);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client()
            .try_withdraw_dust(&stranger, &s.token, &1, &stranger),
        Err(Ok(TreasuryError::Unauthorized))
    );
}

#[test]
fn credit_dust_rejects_zero() {
    let s = setup(0);
    let donor = Address::generate(&s.env);
    assert_eq!(
        s.client().try_credit_dust(&donor, &s.token, &0),
        Err(Ok(TreasuryError::InvalidAmount))
    );
}

// ---------------------------------------------------------------------------
// Merkle batches (issue #278)
// ---------------------------------------------------------------------------

#[test]
fn golden_vector_matches_offchain_tool() {
    // Expected values come from `python3 scripts/merkle_batch.py selftest --print`.
    let env = Env::default();
    let asset = Address::from_string(&String::from_str(
        &env,
        "CCVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKVKUD2U",
    ));
    let recipients = [
        "GAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQDZ7H",
        "GABAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEJXA",
        "GABQGAYDAMBQGAYDAMBQGAYDAMBQGAYDAMBQGAYDAMBQGAYDAMBQHGPC",
        "GACAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAJJHP",
        "GACQKBIFAUCQKBIFAUCQKBIFAUCQKBIFAUCQKBIFAUCQKBIFAUCQKG7N",
    ];
    let hashes: StdVec<BytesN<32>> = recipients
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let n = (i + 1) as u8;
            let recipient = Address::from_string(&String::from_str(&env, r));
            leaf_hash(
                &env,
                &recipient,
                &asset,
                100 * n as i128,
                &BytesN::from_array(&env, &[n; 32]),
            )
        })
        .collect();

    assert_eq!(
        hashes[0].to_array(),
        hex32("ea019b17904e1fb143ed161b2efc8df05a6bb176947e50e86e4696d734258349")
    );
    let levels = build_levels(&env, &hashes);
    assert_eq!(
        levels.last().unwrap()[0].to_array(),
        hex32("0139f0e0fe211658a767ad7c1d7e6242f35e42945d4ed60a93e5b4f2600b7429")
    );
}

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

#[test]
fn publish_claim_sweep_happy_path() {
    let s = setup(1_000);
    let tree = Tree::new(&s.env, &s.token, &[100, 200, 300]);
    let id = s
        .client()
        .publish_batch(&s.admin, &tree.root(), &600, &s.token, &2_000);
    assert_eq!(id, 0);
    assert_last_event(&s.env, "batch_published", id);

    assert_eq!(claim(&s, &tree, id, 0), Ok(100));
    assert_last_event(&s.env, "leaf_claimed", id);
    assert_eq!(claim(&s, &tree, id, 2), Ok(400));
    assert_eq!(s.token_client().balance(&tree.leaves[0].recipient), 100);
    assert_eq!(s.token_client().balance(&tree.leaves[2].recipient), 300);
    assert!(s.client().is_claimed(&id, &tree.leaves[0].hash));
    assert!(!s.client().is_claimed(&id, &tree.leaves[1].hash));
    assert_eq!(s.client().reserved(&s.token), 200);

    s.env.ledger().set_timestamp(2_000);
    assert_eq!(s.client().sweep_unclaimed(&id), 200);
    assert_last_event(&s.env, "batch_swept", id);
    assert_eq!(s.client().reserved(&s.token), 0);
    assert_eq!(s.client().free_balance(&s.token), 600);

    let batch = s.client().get_batch(&id).unwrap();
    assert_eq!(batch.claimed, 400);
    assert!(batch.swept);
}

#[test]
fn publish_batch_unauthorized_is_rejected() {
    let s = setup(1_000);
    let stranger = Address::generate(&s.env);
    let root = BytesN::from_array(&s.env, &[1; 32]);
    assert_eq!(
        s.client()
            .try_publish_batch(&stranger, &root, &100, &s.token, &2_000),
        Err(Ok(TreasuryError::Unauthorized))
    );
}

#[test]
fn publish_validates_inputs() {
    let s = setup(1_000);
    let root = BytesN::from_array(&s.env, &[1; 32]);
    assert_eq!(
        s.client()
            .try_publish_batch(&s.admin, &root, &0, &s.token, &2_000),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        s.client()
            .try_publish_batch(&s.admin, &root, &100, &s.token, &1_000),
        Err(Ok(TreasuryError::InvalidExpiry))
    );
    // Reservation cannot exceed free balance, including across batches.
    s.client()
        .publish_batch(&s.admin, &root, &700, &s.token, &2_000);
    assert_eq!(
        s.client()
            .try_publish_batch(&s.admin, &root, &301, &s.token, &2_000),
        Err(Ok(TreasuryError::InsufficientFreeBalance))
    );
}

#[test]
fn invalid_proofs_are_rejected() {
    let s = setup(1_000);
    let tree = Tree::new(&s.env, &s.token, &[100, 200, 300, 400]);
    let id = s
        .client()
        .publish_batch(&s.admin, &tree.root(), &1_000, &s.token, &2_000);
    let l = &tree.leaves[1];
    let proof = tree.proof(&s.env, 1);

    // Wrong amount.
    assert_eq!(
        s.client()
            .try_claim_leaf(&id, &l.recipient, &(l.amount + 1), &l.salt, &proof),
        Err(Ok(TreasuryError::InvalidProof))
    );
    // Wrong recipient.
    let thief = Address::generate(&s.env);
    assert_eq!(
        s.client()
            .try_claim_leaf(&id, &thief, &l.amount, &l.salt, &proof),
        Err(Ok(TreasuryError::InvalidProof))
    );
    // Wrong salt.
    assert_eq!(
        s.client()
            .try_claim_leaf(&id, &l.recipient, &l.amount, &salt(&s.env, 99), &proof),
        Err(Ok(TreasuryError::InvalidProof))
    );
    // Another leaf's proof.
    assert_eq!(
        s.client().try_claim_leaf(
            &id,
            &l.recipient,
            &l.amount,
            &l.salt,
            &tree.proof(&s.env, 0)
        ),
        Err(Ok(TreasuryError::InvalidProof))
    );
    // Empty proof.
    assert_eq!(
        s.client()
            .try_claim_leaf(&id, &l.recipient, &l.amount, &l.salt, &vec![&s.env]),
        Err(Ok(TreasuryError::InvalidProof))
    );
    // Non-positive amount.
    assert_eq!(
        s.client()
            .try_claim_leaf(&id, &l.recipient, &0, &l.salt, &proof),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    // Unknown batch.
    assert_eq!(
        s.client()
            .try_claim_leaf(&9, &l.recipient, &l.amount, &l.salt, &proof),
        Err(Ok(TreasuryError::BatchNotFound))
    );
}

#[test]
fn overlong_proof_is_rejected() {
    let s = setup(1_000);
    let tree = Tree::new(&s.env, &s.token, &[100]);
    let id = s
        .client()
        .publish_batch(&s.admin, &tree.root(), &100, &s.token, &2_000);
    let mut proof = Vec::new(&s.env);
    for _ in 0..=MAX_PROOF_LEN {
        proof.push_back(BytesN::from_array(&s.env, &[0; 32]));
    }
    let l = &tree.leaves[0];
    assert_eq!(
        s.client()
            .try_claim_leaf(&id, &l.recipient, &l.amount, &l.salt, &proof),
        Err(Ok(TreasuryError::ProofTooLong))
    );
}

#[test]
fn double_claim_is_rejected() {
    let s = setup(1_000);
    let tree = Tree::new(&s.env, &s.token, &[100, 200]);
    let id = s
        .client()
        .publish_batch(&s.admin, &tree.root(), &300, &s.token, &2_000);
    claim(&s, &tree, id, 0).unwrap();
    assert_eq!(
        claim(&s, &tree, id, 0),
        Err(TreasuryError::LeafAlreadyClaimed)
    );
    assert_eq!(s.token_client().balance(&tree.leaves[0].recipient), 100);
}

#[test]
fn duplicate_leaves_pay_only_once() {
    // The off-chain tool rejects duplicates; if one slips through, the
    // claimed flag (keyed by leaf hash) still stops a second payout.
    let s = setup(1_000);
    let recipient = Address::generate(&s.env);
    let sl = salt(&s.env, 1);
    let leaf = leaf_hash(&s.env, &recipient, &s.token, 100, &sl);
    let levels = build_levels(&s.env, &[leaf.clone(), leaf.clone()]);
    let root = levels.last().unwrap()[0].clone();
    let proof = proof_for(&s.env, &levels, &leaf);
    let id = s
        .client()
        .publish_batch(&s.admin, &root, &200, &s.token, &2_000);

    s.client().claim_leaf(&id, &recipient, &100, &sl, &proof);
    assert_eq!(
        s.client()
            .try_claim_leaf(&id, &recipient, &100, &sl, &proof),
        Err(Ok(TreasuryError::LeafAlreadyClaimed))
    );
}

#[test]
fn leaves_summing_above_reserved_total_cannot_overdraw() {
    // Sum-mismatch: the leaves commit 600 but only 500 was reserved. The
    // off-chain `verify` step catches this before publishing; on-chain the
    // claim that would exceed the reservation fails.
    let s = setup(1_000);
    let tree = Tree::new(&s.env, &s.token, &[300, 300]);
    let id = s
        .client()
        .publish_batch(&s.admin, &tree.root(), &500, &s.token, &2_000);
    claim(&s, &tree, id, 0).unwrap();
    assert_eq!(claim(&s, &tree, id, 1), Err(TreasuryError::BatchOverdrawn));
    assert_eq!(s.client().reserved(&s.token), 200);
}

#[test]
fn claims_close_at_expiry_and_sweep_is_idempotent() {
    let s = setup(1_000);
    let tree = Tree::new(&s.env, &s.token, &[100, 200]);
    let id = s
        .client()
        .publish_batch(&s.admin, &tree.root(), &300, &s.token, &2_000);
    assert_eq!(
        s.client().try_sweep_unclaimed(&id),
        Err(Ok(TreasuryError::BatchNotExpired))
    );

    s.env.ledger().set_timestamp(2_000);
    assert_eq!(claim(&s, &tree, id, 0), Err(TreasuryError::BatchExpired));
    assert_eq!(s.client().sweep_unclaimed(&id), 300);
    assert_eq!(s.client().sweep_unclaimed(&id), 0);
    assert_eq!(s.client().reserved(&s.token), 0);
    // Claim after sweep is still closed.
    assert_eq!(claim(&s, &tree, id, 1), Err(TreasuryError::BatchExpired));
    assert_eq!(
        s.client().try_sweep_unclaimed(&7),
        Err(Ok(TreasuryError::BatchNotFound))
    );
}

#[test]
fn stress_three_hundred_leaves_within_budget() {
    let s = setup(1_000_000);
    let amounts: StdVec<i128> = (1..=300).map(|i| i as i128).collect();
    let total: i128 = amounts.iter().sum();
    let tree = Tree::new(&s.env, &s.token, &amounts);
    let id = s
        .client()
        .publish_batch(&s.admin, &tree.root(), &total, &s.token, &2_000);

    let mut max_cpu = 0u64;
    for i in 0..tree.leaves.len() {
        s.env.budget().reset_default();
        claim(&s, &tree, id, i).unwrap();
        max_cpu = max_cpu.max(s.env.budget().cpu_instruction_cost());
    }
    let batch = s.client().get_batch(&id).unwrap();
    assert_eq!(batch.claimed, total);
    assert_eq!(s.client().reserved(&s.token), 0);
    // A 300-leaf proof is 9 hashes deep. Each claim must stay far below the
    // 100M-instruction per-transaction limit.
    std::println!("max cpu instructions per claim (300 leaves): {max_cpu}");
    assert!(max_cpu < 5_000_000, "claim used {max_cpu} instructions");
}

fn assert_last_event(env: &Env, name: &str, batch_id: u64) {
    let (_, topics, _) = env.events().all().last().unwrap();
    let topic0 = Symbol::try_from_val(env, &topics.get(0).unwrap()).unwrap();
    assert_eq!(topic0, Symbol::new(env, name));
    let topic1: Val = topics.get(1).unwrap();
    assert_eq!(u64::try_from_val(env, &topic1).unwrap(), batch_id);
}
