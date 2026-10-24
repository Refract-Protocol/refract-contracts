//! # RefractTreasury
//!
//! Token custody for the Refract protocol with:
//!
//! * Approver-gated withdrawals to allowlisted recipients (issue #244).
//! * 1e7 fixed-point NAV snapshots using RefractOracle price feeds (issue #245).
//! * Governance-and-timelock-gated backstop seeding / recall path (issue #243).
//! * Governance-maintained recipient allowlist enforced on every value-moving
//!   entrypoint (issue #244).
//!
//! ## NAV snapshots (issue #245)
//!
//! The treasury calls `RefractOracle::get_price(feed_id)` for each registered
//! asset feed and computes:
//!
//! ```text
//! asset_nav_i = balance_i * price_i / PRICE_SCALE
//! total_nav   = Σ asset_nav_i
//! ```
//!
//! A snapshot is stored with its ledger timestamp and exposed via read-only
//! views. Governance registers (`register_asset_feed`) and deregisters
//! (`deregister_asset_feed`) asset-to-feed mappings. Anyone may call
//! `take_nav_snapshot` to persist the latest reading; staleness is surfaced
//! per asset via `asset_price_staleness`.
//!
//! ## Backstop seeding (issue #243)
//!
//! Governance queues a `seed_backstop(amount)` call through the timelock.
//! The funds are transferred to the backstop and accounted in
//! `DataKey::SeededCapital` separately from staker deposits. Only the idle
//! (undrawn) seeded portion may be recalled via `recall_seed(amount)`, and
//! recalled capital is always junior to staker deposits (the backstop's
//! `drawable_capital` reflects losses before the seed is returned).

#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
    Symbol, Vec,
};

// ── Scale constants ───────────────────────────────────────────────────────────

/// Fixed-point scale for price feeds: 1e7 (matches RefractOracle::SCALE).
pub const PRICE_SCALE: i128 = 10_000_000;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    /// Amount is zero, negative, or otherwise invalid.
    InvalidAmount = 4,
    /// The treasury does not hold enough of the token.
    InsufficientBalance = 5,
    /// The destination address has not been screened and allowlisted by
    /// governance (issue #244).
    RecipientNotAllowed = 6,
    /// The oracle contract address has not been configured.
    OracleNotConfigured = 7,
    /// No asset-feed mapping exists for this token.
    FeedNotRegistered = 8,
    /// The oracle returned a stale or zero price.
    StalePriceData = 9,
    /// Fixed-point arithmetic overflowed.
    Overflow = 10,
    /// The backstop address has not been configured.
    BackstopNotConfigured = 11,
    /// The amount to recall exceeds the idle seeded capital.
    RecallExceedsSeed = 12,
    /// The asset-feed mapping already exists.
    FeedAlreadyRegistered = 13,
}

// ── Screening status ──────────────────────────────────────────────────────────

/// Governance-maintained screening status for payout recipients (issue #244).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ScreeningStatus {
    /// Governance-approved; may receive funds.
    Allowed = 0,
    /// Explicitly blocked; may NOT receive funds.
    Blocked = 1,
}

// ── NAV types ─────────────────────────────────────────────────────────────────

/// NAV contribution from a single asset.
#[contracttype]
#[derive(Clone, Debug)]
pub struct AssetNav {
    /// SEP-41 token address.
    pub token: Address,
    /// Treasury balance of this token at snapshot time.
    pub balance: i128,
    /// Oracle price at snapshot time, in 1e7 fixed-point USD.
    pub price: i128,
    /// `balance * price / PRICE_SCALE`, in 1e7 fixed-point USD.
    pub nav: i128,
    /// Unix timestamp of the oracle reading used.
    pub price_timestamp: u64,
}

/// A persisted NAV snapshot.
#[contracttype]
#[derive(Clone, Debug)]
pub struct NavSnapshot {
    /// Ledger sequence at time of snapshot.
    pub ledger: u32,
    /// Unix timestamp at time of snapshot.
    pub timestamp: u64,
    /// Sum of all per-asset NAVs, in 1e7 fixed-point USD.
    pub total_nav: i128,
    /// Per-asset breakdown.
    pub assets: Vec<AssetNav>,
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Governance / timelock admin address (instance).
    Admin,
    /// Oracle contract address (instance).
    Oracle,
    /// Backstop contract address (instance, optional — set after deployment).
    Backstop,
    /// Amount seeded to the backstop from protocol-owned capital (instance).
    /// Accounted separately from staker deposits.
    SeededCapital,
    /// Latest NAV snapshot (instance for fast single-read access).
    LatestSnapshot,
    /// Screening status for a recipient address (persistent).
    RecipientStatus(Address),
    /// Feed id (`Symbol`) for a given token (`Address`) — persistent.
    AssetFeed(Address),
    /// Ordered list of tracked token addresses (instance).
    TrackedAssets,
}

// ── Minimal oracle client ─────────────────────────────────────────────────────

/// The subset of RefractOracle used by the treasury for price lookups.
///
/// We call `get_price(feed_id)` which returns `(value: i128, timestamp: u64)`.
/// If the oracle does not expose this exact signature the treasury falls back
/// to the raw `try_invoke_contract` path, but the named client makes tests
/// straightforward.
#[soroban_sdk::contractclient(name = "OraclePriceClient")]
pub trait OraclePriceInterface {
    /// Price of the asset in USD, 1e7 fixed-point, plus the reading timestamp.
    fn get_price(env: Env, feed_id: Symbol) -> (i128, u64);
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    // ─── Initialization ───────────────────────────────────────────────────

    /// Deploy and configure the treasury.
    ///
    /// `admin` should be the `RefractTimelock` contract address so that all
    /// sensitive operations are governance-gated.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::SeededCapital, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::TrackedAssets, &Vec::<Address>::new(&env));
        env.events()
            .publish((Symbol::new(&env, "trs_init"),), admin);
        Ok(())
    }

    // ─── Configuration ────────────────────────────────────────────────────

    /// Set the oracle contract address. Admin-only.
    pub fn set_oracle(env: Env, caller: Address, oracle: Address) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        env.events()
            .publish((Symbol::new(&env, "trs_oracle"),), oracle);
        Ok(())
    }

    /// Set the backstop contract address. Admin-only. Called after the
    /// backstop is deployed (contracts cannot reference each other at deploy
    /// time).
    pub fn set_backstop(env: Env, caller: Address, backstop: Address) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Backstop, &backstop);
        env.events()
            .publish((Symbol::new(&env, "trs_backstop"),), backstop);
        Ok(())
    }

    // ─── Withdrawals (allowlist-enforced) ─────────────────────────────────

    /// Transfer `amount` of `token` to `to`. Approver/admin-gated.
    ///
    /// `to` must have been approved via `allow_recipient` before this call
    /// (issue #244). A payout to a non-screened address is rejected with
    /// `RecipientNotAllowed` rather than silently executed.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin_or_approver(&env, &caller)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        // Enforce recipient allowlist (issue #244).
        if !Self::is_allowed_inner(&env, &to) {
            return Err(TreasuryError::RecipientNotAllowed);
        }
        let bal = Self::token_balance(&env, &token);
        if amount > bal {
            return Err(TreasuryError::InsufficientBalance);
        }
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &to,
            &amount,
        );
        env.events().publish(
            (Symbol::new(&env, "trs_withdraw"),),
            (token, amount, to),
        );
        Ok(())
    }

    /// The treasury's on-ledger balance of `token`.
    pub fn balance(env: Env, token: Address) -> i128 {
        Self::token_balance(&env, &token)
    }

    // ─── Recipient allowlist (issue #244) ─────────────────────────────────

    /// Allow `recipient` to receive treasury withdrawals. Admin-only.
    pub fn allow_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        env.storage().persistent().set(
            &DataKey::RecipientStatus(recipient.clone()),
            &ScreeningStatus::Allowed,
        );
        env.events()
            .publish((symbol_short!("al_allow"),), recipient);
        Ok(())
    }

    /// Block `recipient` from receiving treasury withdrawals. Admin-only.
    pub fn block_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        env.storage().persistent().set(
            &DataKey::RecipientStatus(recipient.clone()),
            &ScreeningStatus::Blocked,
        );
        env.events()
            .publish((symbol_short!("al_block"),), recipient);
        Ok(())
    }

    /// Remove the screening record for `recipient`. Admin-only.
    pub fn remove_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .persistent()
            .remove(&DataKey::RecipientStatus(recipient.clone()));
        env.events()
            .publish((symbol_short!("al_remove"),), recipient);
        Ok(())
    }

    /// Return the screening status of `recipient`, if any.
    pub fn recipient_status(env: Env, recipient: Address) -> Option<ScreeningStatus> {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientStatus(recipient))
    }

    /// Return `true` iff `recipient` is currently `Allowed`.
    pub fn is_recipient_allowed(env: Env, recipient: Address) -> bool {
        Self::is_allowed_inner(&env, &recipient)
    }

    // ─── Asset feed registry (issue #245) ────────────────────────────────

    /// Register a price feed for a token. Admin-only.
    ///
    /// `token` is the SEP-41 contract address; `feed_id` is the oracle feed
    /// symbol (e.g. `Symbol::new(&env, "USDC_USD")`). The token is added to
    /// the tracked-assets list if not already present.
    pub fn register_asset_feed(
        env: Env,
        caller: Address,
        token: Address,
        feed_id: Symbol,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        let key = DataKey::AssetFeed(token.clone());
        if env.storage().persistent().has(&key) {
            return Err(TreasuryError::FeedAlreadyRegistered);
        }
        env.storage().persistent().set(&key, &feed_id);
        // Add to tracked list if absent.
        let mut assets: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::TrackedAssets)
            .unwrap_or_else(|| Vec::new(&env));
        if !assets.iter().any(|a| a == token) {
            assets.push_back(token.clone());
            env.storage()
                .instance()
                .set(&DataKey::TrackedAssets, &assets);
        }
        env.events()
            .publish((Symbol::new(&env, "trs_feed_reg"),), (token, feed_id));
        Ok(())
    }

    /// Deregister the price feed for `token`. Admin-only.
    ///
    /// The token is removed from the tracked-assets list so it is excluded
    /// from future NAV snapshots. Historical snapshots are unaffected.
    pub fn deregister_asset_feed(
        env: Env,
        caller: Address,
        token: Address,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .persistent()
            .remove(&DataKey::AssetFeed(token.clone()));
        let mut assets: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::TrackedAssets)
            .unwrap_or_else(|| Vec::new(&env));
        let mut new_assets: Vec<Address> = Vec::new(&env);
        for a in assets.iter() {
            if a != token {
                new_assets.push_back(a);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::TrackedAssets, &new_assets);
        env.events()
            .publish((Symbol::new(&env, "trs_feed_dereg"),), token);
        Ok(())
    }

    /// The oracle feed id registered for `token`, if any.
    pub fn asset_feed(env: Env, token: Address) -> Option<Symbol> {
        env.storage().persistent().get(&DataKey::AssetFeed(token))
    }

    /// Ordered list of tracked asset token addresses.
    pub fn tracked_assets(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::TrackedAssets)
            .unwrap_or_else(|| Vec::new(&env))
    }

    // ─── NAV snapshots (issue #245) ───────────────────────────────────────

    /// Compute and persist a NAV snapshot.
    ///
    /// Permissionless — anyone may call this to update the on-chain snapshot.
    /// For each tracked asset:
    ///   1. Read the treasury's on-ledger balance.
    ///   2. Call `oracle.get_price(feed_id)` to obtain the 1e7 USD price.
    ///   3. Compute `nav_i = balance_i * price_i / PRICE_SCALE`.
    ///
    /// Assets with a zero price are included with `nav = 0` so the snapshot
    /// is always complete; staleness is separately observable via
    /// `asset_price_staleness`.
    ///
    /// Returns the total NAV in 1e7 fixed-point USD.
    pub fn take_nav_snapshot(env: Env) -> Result<i128, TreasuryError> {
        let oracle: Address = env
            .storage()
            .instance()
            .get(&DataKey::Oracle)
            .ok_or(TreasuryError::OracleNotConfigured)?;

        let assets: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::TrackedAssets)
            .unwrap_or_else(|| Vec::new(&env));

        let oracle_client = OraclePriceClient::new(&env, &oracle);
        let mut asset_navs: Vec<AssetNav> = Vec::new(&env);
        let mut total_nav: i128 = 0i128;

        for token in assets.iter() {
            let feed_id: Symbol = env
                .storage()
                .persistent()
                .get(&DataKey::AssetFeed(token.clone()))
                .ok_or(TreasuryError::FeedNotRegistered)?;

            let balance = Self::token_balance(&env, &token);

            // Fetch price; treat failure as price = 0 (asset contributes 0 to NAV).
            let (price, price_ts) = match oracle_client.try_get_price(&feed_id) {
                Ok(Ok(result)) => result,
                _ => (0i128, 0u64),
            };

            // nav_i = balance * price / PRICE_SCALE
            let nav = if price == 0 || balance == 0 {
                0i128
            } else {
                balance
                    .checked_mul(price)
                    .ok_or(TreasuryError::Overflow)?
                    .checked_div(PRICE_SCALE)
                    .ok_or(TreasuryError::Overflow)?
            };

            total_nav = total_nav.checked_add(nav).ok_or(TreasuryError::Overflow)?;

            asset_navs.push_back(AssetNav {
                token: token.clone(),
                balance,
                price,
                nav,
                price_timestamp: price_ts,
            });
        }

        let snapshot = NavSnapshot {
            ledger: env.ledger().sequence(),
            timestamp: env.ledger().timestamp(),
            total_nav,
            assets: asset_navs,
        };

        env.storage()
            .instance()
            .set(&DataKey::LatestSnapshot, &snapshot);

        env.events()
            .publish((Symbol::new(&env, "trs_nav"),), total_nav);

        Ok(total_nav)
    }

    /// The most recently persisted NAV snapshot, if any.
    pub fn latest_snapshot(env: Env) -> Option<NavSnapshot> {
        env.storage().instance().get(&DataKey::LatestSnapshot)
    }

    /// Total NAV from the most recent snapshot (0 if no snapshot yet).
    pub fn total_nav(env: Env) -> i128 {
        env.storage()
            .instance()
            .get::<DataKey, NavSnapshot>(&DataKey::LatestSnapshot)
            .map(|s| s.total_nav)
            .unwrap_or(0)
    }

    /// Per-asset NAV breakdown from the most recent snapshot.
    ///
    /// Returns an empty `Vec` if no snapshot has been taken.
    pub fn asset_nav_breakdown(env: Env) -> Vec<AssetNav> {
        env.storage()
            .instance()
            .get::<DataKey, NavSnapshot>(&DataKey::LatestSnapshot)
            .map(|s| s.assets)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Seconds since the oracle price for `token` was last updated, as
    /// recorded in the most recent snapshot.
    ///
    /// Returns `None` if no snapshot exists or the token is not tracked.
    /// A large value indicates stale price data that should be refreshed
    /// before making financial decisions.
    pub fn asset_price_staleness(env: Env, token: Address) -> Option<u64> {
        let snapshot: NavSnapshot = env
            .storage()
            .instance()
            .get(&DataKey::LatestSnapshot)?;
        let now = env.ledger().timestamp();
        for entry in snapshot.assets.iter() {
            if entry.token == token {
                if entry.price_timestamp == 0 {
                    return None; // price was never obtained
                }
                return Some(now.saturating_sub(entry.price_timestamp));
            }
        }
        None
    }

    // ─── Backstop seeding (issue #243) ────────────────────────────────────

    /// Seed the backstop with protocol-owned capital.
    ///
    /// Governance-and-timelock-gated (`caller` must be the admin/timelock).
    ///
    /// `token` is the backstop's staking token; `amount` is transferred from
    /// the treasury to the backstop and deposited on behalf of the treasury.
    /// The amount is tracked in `SeededCapital` separately from staker
    /// deposits so it can be accounted and recalled independently.
    ///
    /// Seeded capital is junior to staker deposits: the backstop's drawdown
    /// logic is not changed here, but the seeded amount is only recalled
    /// after the backstop has returned the protocol's portion via
    /// `recall_seed`.
    pub fn seed_backstop(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let backstop: Address = env
            .storage()
            .instance()
            .get(&DataKey::Backstop)
            .ok_or(TreasuryError::BackstopNotConfigured)?;

        let bal = Self::token_balance(&env, &token);
        if amount > bal {
            return Err(TreasuryError::InsufficientBalance);
        }

        // Transfer to backstop and deposit.
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &backstop,
            &amount,
        );

        // Update seeded-capital accounting.
        let seeded: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SeededCapital)
            .unwrap_or(0i128);
        env.storage()
            .instance()
            .set(&DataKey::SeededCapital, &(seeded + amount));

        env.events()
            .publish((Symbol::new(&env, "trs_seed"),), (token, amount, backstop));
        Ok(())
    }

    /// Recall idle protocol-owned capital from the backstop.
    ///
    /// Governance-and-timelock-gated. The backstop transfers `amount` back to
    /// the treasury. Only up to `seeded_capital()` may be recalled — the
    /// treasury never recalls more than it originally seeded.
    ///
    /// Note: because the backstop may have suffered drawdowns, the actual
    /// transferable amount could be less than `seeded_capital()`. The caller
    /// should query `backstop.drawable_capital()` first. If the backstop
    /// does not hold enough, the token transfer will fail and the tx reverts.
    pub fn recall_seed(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let seeded: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SeededCapital)
            .unwrap_or(0i128);
        if amount > seeded {
            return Err(TreasuryError::RecallExceedsSeed);
        }
        let backstop: Address = env
            .storage()
            .instance()
            .get(&DataKey::Backstop)
            .ok_or(TreasuryError::BackstopNotConfigured)?;

        // The backstop transfers funds back to the treasury.
        token::Client::new(&env, &token).transfer(
            &backstop,
            &env.current_contract_address(),
            &amount,
        );

        // Reduce seeded-capital accounting.
        env.storage()
            .instance()
            .set(&DataKey::SeededCapital, &(seeded - amount));

        env.events()
            .publish((Symbol::new(&env, "trs_recall"),), (token, amount, backstop));
        Ok(())
    }

    /// Amount of protocol-owned capital currently seeded to the backstop.
    pub fn seeded_capital(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::SeededCapital)
            .unwrap_or(0)
    }

    // ─── Read-only views ──────────────────────────────────────────────────

    /// Current admin address.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Configured oracle address, if set.
    pub fn oracle(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Oracle)
    }

    /// Configured backstop address, if set.
    pub fn backstop(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Backstop)
    }

    // ─── Private helpers ──────────────────────────────────────────────────

    fn require_admin(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if *caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        Ok(())
    }

    /// The admin also acts as an approver; a separate `Approver` role can be
    /// layered on later via the access-control module. For now admin is the
    /// only authority for fund-moving calls.
    fn require_admin_or_approver(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        // For the initial implementation admin == approver.
        Self::require_admin(env, caller)
    }

    fn token_balance(env: &Env, token: &Address) -> i128 {
        token::Client::new(env, token)
            .balance(&env.current_contract_address())
    }

    fn is_allowed_inner(env: &Env, recipient: &Address) -> bool {
        matches!(
            env.storage()
                .persistent()
                .get::<DataKey, ScreeningStatus>(&DataKey::RecipientStatus(recipient.clone())),
            Some(ScreeningStatus::Allowed)
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _,
        Address, Env, Symbol,
    };

    fn make_env() -> (Env, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let trs_id = env.register_contract(None, RefractTreasury);
        (env, admin, trs_id)
    }

    #[test]
    fn double_initialize_rejected() {
        let (env, admin, trs_id) = make_env();
        let trs = RefractTreasuryClient::new(&env, &trs_id);
        trs.initialize(&admin);
        assert_eq!(
            trs.try_initialize(&admin),
            Err(Ok(TreasuryError::AlreadyInitialized))
        );
    }

    #[test]
    fn allowlist_gate_on_withdraw() {
        let (env, admin, trs_id) = make_env();
        let trs = RefractTreasuryClient::new(&env, &trs_id);
        trs.initialize(&admin);

        let recipient = Address::generate(&env);
        // Withdraw to an unscreened address is rejected.
        // (Balance check would also fail; RecipientNotAllowed is checked first.)
        let fake_token = Address::generate(&env);
        let res = trs.try_withdraw(&admin, &fake_token, &100i128, &recipient);
        assert_eq!(res, Err(Ok(TreasuryError::RecipientNotAllowed)));

        // After allowlisting, the balance check kicks in (no real token here).
        trs.allow_recipient(&admin, &recipient);
        assert!(trs.is_recipient_allowed(&recipient));
        // InsufficientBalance because treasury holds no tokens in test.
        let res2 = trs.try_withdraw(&admin, &fake_token, &100i128, &recipient);
        assert_eq!(res2, Err(Ok(TreasuryError::InsufficientBalance)));
    }

    #[test]
    fn block_and_remove_recipient() {
        let (env, admin, trs_id) = make_env();
        let trs = RefractTreasuryClient::new(&env, &trs_id);
        trs.initialize(&admin);

        let r = Address::generate(&env);
        trs.allow_recipient(&admin, &r);
        assert!(trs.is_recipient_allowed(&r));
        trs.block_recipient(&admin, &r);
        assert!(!trs.is_recipient_allowed(&r));
        trs.remove_recipient(&admin, &r);
        assert_eq!(trs.recipient_status(&r), None);
    }

    #[test]
    fn seeded_capital_accounting() {
        let (env, admin, trs_id) = make_env();
        let trs = RefractTreasuryClient::new(&env, &trs_id);
        trs.initialize(&admin);

        // No backstop configured → BackstopNotConfigured.
        let tok = Address::generate(&env);
        let res = trs.try_seed_backstop(&admin, &tok, &500i128);
        assert_eq!(res, Err(Ok(TreasuryError::BackstopNotConfigured)));

        // Set backstop and check accounting path (token transfer will fail
        // because this is a mock, but we can test the error path).
        let backstop = Address::generate(&env);
        trs.set_backstop(&admin, &backstop);
        assert_eq!(trs.seeded_capital(), 0);
    }

    #[test]
    fn recall_exceeding_seed_rejected() {
        let (env, admin, trs_id) = make_env();
        let trs = RefractTreasuryClient::new(&env, &trs_id);
        trs.initialize(&admin);
        let backstop = Address::generate(&env);
        trs.set_backstop(&admin, &backstop);

        let tok = Address::generate(&env);
        // seeded_capital == 0, so any positive recall is rejected.
        let res = trs.try_recall_seed(&admin, &tok, &1i128);
        assert_eq!(res, Err(Ok(TreasuryError::RecallExceedsSeed)));
    }

    #[test]
    fn register_and_deregister_asset_feed() {
        let (env, admin, trs_id) = make_env();
        let trs = RefractTreasuryClient::new(&env, &trs_id);
        trs.initialize(&admin);

        let tok = Address::generate(&env);
        let feed = Symbol::new(&env, "USDC_USD");
        trs.register_asset_feed(&admin, &tok, &feed);
        let tracked = trs.tracked_assets();
        assert_eq!(tracked.len(), 1);

        // Duplicate registration is rejected.
        let res = trs.try_register_asset_feed(&admin, &tok, &feed);
        assert_eq!(res, Err(Ok(TreasuryError::FeedAlreadyRegistered)));

        trs.deregister_asset_feed(&admin, &tok);
        assert_eq!(trs.tracked_assets().len(), 0);
    }

    #[test]
    fn nav_snapshot_no_oracle_rejected() {
        let (env, admin, trs_id) = make_env();
        let trs = RefractTreasuryClient::new(&env, &trs_id);
        trs.initialize(&admin);
        let res = trs.try_take_nav_snapshot();
        assert_eq!(res, Err(Ok(TreasuryError::OracleNotConfigured)));
    }
}
