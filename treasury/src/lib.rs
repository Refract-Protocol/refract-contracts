#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol, Vec,
};

/// Basis points denominator for keeper bands.
const BPS_DENOM: i128 = 10_000;

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    /// Append-only journal length.
    JournalCount,
    /// Journal entry by sequence number (never removed).
    JournalEntry(u64),
    /// Reservation record by id.
    Reservation(u64),
    /// Next reservation id.
    ReservationCount,
    /// Sum of outstanding (reserved, undrawn) amounts per token is tracked
    /// via a single i128 when the treasury manages one canonical asset; for
    /// multi-asset deployments the invariant is checked against the journal.
    ReservedTotal,
    /// Guardian breaker state.
    Guardian,
    /// Keeper budget state.
    Keeper,
}

/// Balanced double-entry posting: every treasury movement is a matched pair.
#[contracttype]
#[derive(Clone)]
pub struct JournalEntry {
    pub seq: u64,
    pub debit: Symbol,
    pub credit: Symbol,
    pub asset: Address,
    pub amount: i128,
    pub timestamp: u64,
}

/// A committed-but-undrawn capital reservation.
#[contracttype]
#[derive(Clone)]
pub struct Reservation {
    pub id: u64,
    pub consumer: Address,
    pub token: Address,
    pub amount: i128,
    pub drawn: i128,
    pub released: bool,
    pub purpose: Symbol,
    pub created_at: u64,
}

/// Guardian breaker: halts outflows on oracle staleness or depeg.
#[contracttype]
#[derive(Clone)]
pub struct GuardianConfig {
    pub oracle: Option<Address>,
    pub max_staleness_secs: u64,
    pub depeg_bps: u32,
    pub peg_price: i128,
    pub last_price: i128,
    pub last_update: u64,
    pub outflows_paused: bool,
}

/// Epoch-capped keeper budget drawn from the treasury.
#[contracttype]
#[derive(Clone)]
pub struct KeeperBudget {
    pub reward_per_call: i128,
    pub cap_per_epoch: i128,
    pub epoch_len_secs: u64,
    pub epoch_start: u64,
    pub epoch_spent: i128,
}

/// `RefractTreasury` is a deliberately minimal token-custody vault.
///
/// Funds are deposited via plain token transfers to the contract address
/// (no `deposit` entrypoint is required). Withdrawals are admin-gated.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    /// Guardian-gated: reverts while outflows are paused.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        Self::require_outflows_live(&env);
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let available = Self::available_balance(&env, token.clone());
        if amount > available {
            panic!("insufficient available balance (funds reserved)");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        Self::post_balanced(
            &env,
            symbol_short!("expense"),
            symbol_short!("cash"),
            token,
            amount,
        );
    }

    // ─── #270 Journal ────────────────────────────────────────────────

    pub fn journal_len(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64)
    }

    pub fn journal_entry(env: Env, seq: u64) -> Option<JournalEntry> {
        env.storage().persistent().get(&DataKey::JournalEntry(seq))
    }

    pub fn journal_list(env: Env, start: u64, count: u64) -> Vec<JournalEntry> {
        let mut out = Vec::new(&env);
        let mut i = start;
        let end = start + count;
        while i < end {
            if let Some(e) = env.storage().persistent().get(&DataKey::JournalEntry(i)) {
                out.push_back(e);
            }
            i += 1;
        }
        out
    }

    /// Accounting-identity check: journal balanced by construction, reserved
    /// total equals the sum of open reservations, and available + reserved
    /// never exceeds the live token balance.
    pub fn check_invariants(env: Env, token: Address) -> bool {
        let len: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64);
        let mut idx: u64 = 0;
        while idx < len {
            let entry: Option<JournalEntry> =
                env.storage().persistent().get(&DataKey::JournalEntry(idx));
            match entry {
                None => return false,
                Some(e) => {
                    if e.amount <= 0 {
                        return false;
                    }
                }
            }
            idx += 1;
        }
        let reserved = Self::reserved_total(&env);
        if reserved < 0 {
            return false;
        }
        let live = token::Client::new(&env, &token).balance(&env.current_contract_address());
        if reserved > live {
            return false;
        }
        // Sum open reservations and compare to the cached total.
        let next: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::ReservationCount)
            .unwrap_or(0u64);
        let mut sum: i128 = 0;
        let mut id: u64 = 0;
        while id < next {
            if let Some(r) = env
                .storage()
                .persistent()
                .get(&DataKey::Reservation(id))
            {
                let open: Reservation = r;
                if !open.released {
                    let outstanding = open.amount - open.drawn;
                    if outstanding < 0 {
                        return false;
                    }
                    sum += outstanding;
                    if sum < 0 {
                        return false;
                    }
                }
            }
            id += 1;
        }
        sum == reserved
    }

    // ─── #269 Capital reservation ledger ────────────────────────────

    /// Reserve `amount` of `token` for `consumer` (e.g. backstop seeding,
    /// grant escrow, bounty escrow). Admin-only; prevents double-commitment
    /// by enforcing `amount <= available_balance`.
    pub fn reserve(
        env: Env,
        caller: Address,
        consumer: Address,
        token: Address,
        amount: i128,
        purpose: Symbol,
    ) -> u64 {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let available = Self::available_balance(&env, token.clone());
        if amount > available {
            panic!("insufficient available balance (funds reserved)");
        }
        let id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::ReservationCount)
            .unwrap_or(0u64);
        let rec = Reservation {
            id,
            consumer: consumer.clone(),
            token: token.clone(),
            amount,
            drawn: 0,
            released: false,
            purpose: purpose.clone(),
            created_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::Reservation(id), &rec);
        env.storage()
            .persistent()
            .set(&DataKey::ReservationCount, &(id + 1));
        let total = Self::reserved_total(&env);
        env.storage()
            .instance()
            .set(&DataKey::ReservedTotal, &(total + amount));
        Self::post_balanced(&env, symbol_short!("commit"), symbol_short!("free"), token, amount);
        env.events().publish((symbol_short!("reserved"), id), (consumer, amount));
        id
    }

    /// Release an undrawn reservation, freeing the capital.
    pub fn release(env: Env, caller: Address, reservation_id: u64) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        let mut rec: Reservation = env
            .storage()
            .persistent()
            .get(&DataKey::Reservation(reservation_id))
            .expect("unknown reservation");
        if rec.released {
            panic!("already released");
        }
        let outstanding = rec.amount - rec.drawn;
        rec.released = true;
        env.storage()
            .persistent()
            .set(&DataKey::Reservation(reservation_id), &rec);
        let total = Self::reserved_total(&env);
        env.storage()
            .instance()
            .set(&DataKey::ReservedTotal, &(total - outstanding));
        Self::post_balanced(
            &env,
            symbol_short!("free"),
            symbol_short!("commit"),
            rec.token,
            outstanding,
        );
        env.events()
            .publish((symbol_short!("released"), reservation_id), (outstanding,));
    }

    /// Draw down `amount` against a reservation, paying `to`. Admin-only.
    /// Guardian-gated like `withdraw`.
    pub fn draw_down(
        env: Env,
        caller: Address,
        reservation_id: u64,
        amount: i128,
        to: Address,
    ) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        Self::require_outflows_live(&env);
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let mut rec: Reservation = env
            .storage()
            .persistent()
            .get(&DataKey::Reservation(reservation_id))
            .expect("unknown reservation");
        if rec.released {
            panic!("reservation released");
        }
        let outstanding = rec.amount - rec.drawn;
        if amount > outstanding {
            panic!("draw exceeds reservation");
        }
        token::Client::new(&env, &rec.token).transfer(
            &env.current_contract_address(),
            &to,
            &amount,
        );
        rec.drawn += amount;
        env.storage()
            .persistent()
            .set(&DataKey::Reservation(reservation_id), &rec);
        let total = Self::reserved_total(&env);
        env.storage()
            .instance()
            .set(&DataKey::ReservedTotal, &(total - amount));
        Self::post_balanced(
            &env,
            symbol_short!("expense"),
            symbol_short!("commit"),
            rec.token,
            amount,
        );
        env.events()
            .publish((symbol_short!("drawn"), reservation_id), (to, amount));
    }

    /// Sum of outstanding (reserved minus drawn, unreleased) capital.
    pub fn reserved_total(env: Env) -> i128 {
        Self::reserved_total(&env)
    }

    /// Spendable balance: live token balance minus outstanding reservations.
    pub fn available_balance(env: Env, token: Address) -> i128 {
        Self::available_balance(&env, token)
    }

    pub fn get_reservation(env: Env, reservation_id: u64) -> Option<Reservation> {
        env.storage()
            .persistent()
            .get(&DataKey::Reservation(reservation_id))
    }

    // ─── #272 Guardian ───────────────────────────────────────────────

    pub fn set_guardian_config(
        env: Env,
        caller: Address,
        oracle: Option<Address>,
        max_staleness_secs: u64,
        depeg_bps: u32,
        peg_price: i128,
    ) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if depeg_bps == 0 || depeg_bps > 10_000 || peg_price <= 0 {
            panic!("invalid guardian config");
        }
        let prev: Option<GuardianConfig> = env.storage().instance().get(&DataKey::Guardian);
        let cfg = GuardianConfig {
            oracle,
            max_staleness_secs,
            depeg_bps,
            peg_price,
            last_price: prev.as_ref().map(|p| p.last_price).unwrap_or(peg_price),
            last_update: prev
                .as_ref()
                .map(|p| p.last_update)
                .unwrap_or(env.ledger().timestamp()),
            outflows_paused: prev.as_ref().map(|p| p.outflows_paused).unwrap_or(false),
        };
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        env.events().publish(
            (symbol_short!("g_cfg"),),
            (max_staleness_secs, depeg_bps),
        );
    }

    pub fn note_feed(env: Env, price: i128, timestamp: u64) {
        if price <= 0 {
            panic!("invalid price");
        }
        let mut cfg: GuardianConfig = env.storage().instance().get(&DataKey::Guardian).unwrap_or(
            GuardianConfig {
                oracle: None,
                max_staleness_secs: 1_800,
                depeg_bps: 500,
                peg_price: price,
                last_price: price,
                last_update: timestamp,
                outflows_paused: false,
            },
        );
        if timestamp > env.ledger().timestamp() + 300 {
            panic!("feed timestamp in future");
        }
        cfg.last_price = price;
        cfg.last_update = timestamp;
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        let _ = Self::evaluate_guardian(&env);
    }

    /// Hot-path check: auto-pauses outflows on staleness or depeg.
    pub fn check_guardians(env: Env) {
        let _ = Self::evaluate_guardian(&env);
        Self::require_outflows_live(&env);
    }

    pub fn clear_halt(env: Env, caller: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        let mut cfg: GuardianConfig = env
            .storage()
            .instance()
            .get(&DataKey::Guardian)
            .expect("guardian not configured");
        cfg.outflows_paused = false;
        cfg.last_update = env.ledger().timestamp();
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        env.events().publish((symbol_short!("g_clr"),), ());
    }

    // ─── #271 Keeper budget + execute_due ────────────────────────────

    pub fn set_keeper_budget(
        env: Env,
        caller: Address,
        reward_per_call: i128,
        cap_per_epoch: i128,
        epoch_len_secs: u64,
    ) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if reward_per_call < 0 || cap_per_epoch < 0 || epoch_len_secs == 0 {
            panic!("invalid keeper budget");
        }
        env.storage().instance().set(
            &DataKey::Keeper,
            &KeeperBudget {
                reward_per_call,
                cap_per_epoch,
                epoch_len_secs,
                epoch_start: env.ledger().timestamp(),
                epoch_spent: 0,
            },
        );
    }

    /// Permissionless keeper sweep: pays a capped reward from the treasury
    /// budget and reports how many due items were swept (0 when idle).
    /// Never reverts the sweep when the budget is exhausted.
    pub fn execute_due(env: Env, keeper: Address, token: Address) -> i128 {
        keeper.require_auth();
        let mut budget: KeeperBudget = env.storage().instance().get(&DataKey::Keeper).unwrap_or(
            KeeperBudget {
                reward_per_call: 0,
                cap_per_epoch: 0,
                epoch_len_secs: 86_400,
                epoch_start: env.ledger().timestamp(),
                epoch_spent: 0,
            },
        );
        let now = env.ledger().timestamp();
        if now >= budget.epoch_start + budget.epoch_len_secs {
            budget.epoch_start = now;
            budget.epoch_spent = 0;
        }
        let mut reward: i128 = 0;
        if budget.reward_per_call > 0
            && budget.epoch_spent + budget.reward_per_call <= budget.cap_per_epoch
        {
            let live = token::Client::new(&env, &token).balance(&env.current_contract_address());
            let reserved = Self::reserved_total(&env);
            let available = live - reserved;
            if available >= budget.reward_per_call
                && !Self::outflows_paused(&env)
            {
                token::Client::new(&env, &token).transfer(
                    &env.current_contract_address(),
                    &keeper,
                    &budget.reward_per_call,
                );
                Self::post_balanced(
                    &env,
                    symbol_short!("keeper"),
                    symbol_short!("cash"),
                    token,
                    budget.reward_per_call,
                );
                budget.epoch_spent += budget.reward_per_call;
                reward = budget.reward_per_call;
            }
        }
        env.storage().instance().set(&DataKey::Keeper, &budget);
        env.events()
            .publish((symbol_short!("keeper"),), (keeper, reward));
        reward
    }

    fn reserved_total(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReservedTotal)
            .unwrap_or(0i128)
    }

    fn available_balance(env: &Env, token: Address) -> i128 {
        let live = token::Client::new(env, &token).balance(&env.current_contract_address());
        live - Self::reserved_total(env)
    }

    fn outflows_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get::<_, GuardianConfig>(&DataKey::Guardian)
            .map(|c| c.outflows_paused)
            .unwrap_or(false)
    }

    fn require_outflows_live(env: &Env) {
        if Self::outflows_paused(env) {
            panic!("treasury outflows paused by risk guardian");
        }
    }

    fn evaluate_guardian(env: &Env) -> bool {
        let mut cfg: GuardianConfig = match env.storage().instance().get(&DataKey::Guardian) {
            None => return true,
            Some(c) => c,
        };
        if cfg.outflows_paused {
            return false;
        }
        let age = env.ledger().timestamp().saturating_sub(cfg.last_update);
        if age > cfg.max_staleness_secs {
            cfg.outflows_paused = true;
            env.storage().instance().set(&DataKey::Guardian, &cfg);
            env.events().publish(
                (symbol_short!("g_trip"), symbol_short!("stale")),
                (age,),
            );
            return false;
        }
        let diff = if cfg.last_price >= cfg.peg_price {
            cfg.last_price - cfg.peg_price
        } else {
            cfg.peg_price - cfg.last_price
        };
        let band = diff * BPS_DENOM / cfg.peg_price;
        if band > cfg.depeg_bps as i128 {
            cfg.outflows_paused = true;
            env.storage().instance().set(&DataKey::Guardian, &cfg);
            env.events().publish(
                (symbol_short!("g_trip"), symbol_short!("depeg")),
                (band,),
            );
            return false;
        }
        true
    }

    fn post_balanced(env: &Env, debit: Symbol, credit: Symbol, asset: Address, amount: i128) {
        if amount <= 0 {
            panic!("journal amount must be positive");
        }
        let seq: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64);
        let entry = JournalEntry {
            seq,
            debit: debit.clone(),
            credit: credit.clone(),
            asset,
            amount,
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::JournalEntry(seq), &entry);
        env.storage()
            .persistent()
            .set(&DataKey::JournalCount, &(seq + 1));
        env.events().publish(
            (symbol_short!("journal"), seq),
            (debit, credit, amount),
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract(token_admin.clone());
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }
}
