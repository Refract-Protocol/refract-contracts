//! # RefractBackstop
//!
//! Second-loss reinsurance/backstop capital tranche with an on-chain withdrawal
//! queue (`request_withdraw`, `cancel_withdraw`, `claim_withdraw`) and epoch-level
//! pro-rata fills (`settle_epoch`) bounded by the solvency floor and loss-event freeze.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

const BPS_DENOM: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    BelowMinRequestSize = 8,
    OpenOrderCapExceeded = 9,
    OrderNotFound = 10,
    CannotCancelAfterPartialFill = 11,
    LossEventFreezeActive = 12,
    NothingToClaim = 13,
    InvalidOrderState = 14,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    QueuePolicy,
    Shares(Address),
    OpenOrders(Address),
    TotalQueuedShares,
    NextOrderId,
    Order(u64),
    OrderIds,
    CurrentEpoch,
    EpochSnapshot(u64),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuePolicy {
    pub cooldown_ledgers: u32,
    pub min_request_shares: i128,
    pub max_open_orders_per_staker: u32,
    pub solvency_floor: i128,
    pub loss_event_active: bool,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum OrderStatus {
    Queued = 0,
    PartiallyFilled = 1,
    Filled = 2,
    Cancelled = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawOrder {
    pub id: u64,
    pub staker: Address,
    pub initial_shares: i128,
    pub remaining_shares: i128,
    pub claimable_principal: i128,
    pub claimed_principal: i128,
    pub requested_at: u32,
    pub eligible_at: u32,
    pub status: OrderStatus,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochSnapshot {
    pub epoch: u64,
    pub total_eligible_shares: i128,
    pub total_filled_shares: i128,
    pub total_fillable_amount: i128,
    pub fill_ratio_bps: i128,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool,
            admin,
            token,
            premium_share_bps,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(
            &DataKey::QueuePolicy,
            &QueuePolicy {
                cooldown_ledgers: 5,
                min_request_shares: 100,
                max_open_orders_per_staker: 3,
                solvency_floor: 1_000,
                loss_event_active: false,
            },
        );
        env.storage()
            .instance()
            .set(&DataKey::TotalQueuedShares, &0i128);
        env.storage().instance().set(&DataKey::NextOrderId, &0u64);
        env.storage().instance().set(&DataKey::CurrentEpoch, &0u64);
        Ok(())
    }

    pub fn set_queue_policy(
        env: Env,
        cooldown_ledgers: u32,
        min_request_shares: i128,
        max_open_orders_per_staker: u32,
        solvency_floor: i128,
        loss_event_active: bool,
    ) -> Result<QueuePolicy, BackstopError> {
        let cfg = Self::read_config(&env)?;
        cfg.admin.require_auth();
        if min_request_shares <= 0 || max_open_orders_per_staker == 0 || solvency_floor < 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let policy = QueuePolicy {
            cooldown_ledgers,
            min_request_shares,
            max_open_orders_per_staker,
            solvency_floor,
            loss_event_active,
        };
        env.storage().instance().set(&DataKey::QueuePolicy, &policy);
        Ok(policy)
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = if state.total_shares == 0 || state.total_capital == 0 {
            amount
        } else {
            amount * state.total_shares / state.total_capital
        };
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let prev: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Shares(staker), &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);
        Ok(shares)
    }

    /// Escrows `shares` into a timestamped withdrawal order in the queue.
    pub fn request_withdraw(env: Env, staker: Address, shares: i128) -> Result<u64, BackstopError> {
        staker.require_auth();
        let policy = Self::read_policy(&env)?;
        if shares < policy.min_request_shares {
            return Err(BackstopError::BelowMinRequestSize);
        }

        let open_count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::OpenOrders(staker.clone()))
            .unwrap_or(0);
        if open_count >= policy.max_open_orders_per_staker {
            return Err(BackstopError::OpenOrderCapExceeded);
        }

        let held: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Shares(staker.clone()), &(held - shares));
        env.storage()
            .persistent()
            .set(&DataKey::OpenOrders(staker.clone()), &(open_count + 1));

        let queued: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalQueuedShares)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalQueuedShares, &(queued + shares));

        let order_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextOrderId)
            .unwrap_or(0);
        let now = env.ledger().sequence();
        let eligible_at = now.saturating_add(policy.cooldown_ledgers);
        let order = WithdrawOrder {
            id: order_id,
            staker: staker.clone(),
            initial_shares: shares,
            remaining_shares: shares,
            claimable_principal: 0,
            claimed_principal: 0,
            requested_at: now,
            eligible_at,
            status: OrderStatus::Queued,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Order(order_id), &order);
        env.storage()
            .instance()
            .set(&DataKey::NextOrderId, &(order_id + 1));

        let mut ids: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::OrderIds)
            .unwrap_or_else(|| Vec::new(&env));
        ids.push_back(order_id);
        env.storage().instance().set(&DataKey::OrderIds, &ids);

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("wq_req")),
            (order_id, staker, shares, eligible_at),
        );
        Ok(order_id)
    }

    /// Cancels a queued withdrawal order before any partial fill has occurred.
    pub fn cancel_withdraw(
        env: Env,
        staker: Address,
        order_id: u64,
    ) -> Result<WithdrawOrder, BackstopError> {
        staker.require_auth();
        let mut order: WithdrawOrder = env
            .storage()
            .persistent()
            .get(&DataKey::Order(order_id))
            .ok_or(BackstopError::OrderNotFound)?;
        if order.staker != staker {
            return Err(BackstopError::Unauthorized);
        }
        if order.status != OrderStatus::Queued || order.remaining_shares != order.initial_shares {
            return Err(BackstopError::CannotCancelAfterPartialFill);
        }

        let held: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::Shares(staker.clone()),
            &(held + order.remaining_shares),
        );

        let open_count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::OpenOrders(staker.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::OpenOrders(staker), &open_count.saturating_sub(1));

        let queued: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalQueuedShares)
            .unwrap_or(0);
        env.storage().instance().set(
            &DataKey::TotalQueuedShares,
            &(queued - order.remaining_shares),
        );

        order.remaining_shares = 0;
        order.status = OrderStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Order(order_id), &order);
        Ok(order)
    }

    /// Settles an epoch pro-rata across eligible orders without breaching `solvency_floor`.
    /// Freezes fills if a loss event is currently active.
    pub fn settle_epoch(env: Env, caller: Address) -> Result<EpochSnapshot, BackstopError> {
        caller.require_auth();
        let policy = Self::read_policy(&env)?;
        if policy.loss_event_active {
            return Err(BackstopError::LossEventFreezeActive);
        }

        let mut state = Self::read_state(&env)?;
        let available_liquidity = (state.total_capital - policy.solvency_floor).max(0);
        let now = env.ledger().sequence();

        let ids: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::OrderIds)
            .unwrap_or_else(|| Vec::new(&env));

        let mut eligible_orders: Vec<WithdrawOrder> = Vec::new(&env);
        let mut total_eligible_shares: i128 = 0;
        for id in ids.iter() {
            if let Some(ord) = env
                .storage()
                .persistent()
                .get::<DataKey, WithdrawOrder>(&DataKey::Order(id))
            {
                if (ord.status == OrderStatus::Queued || ord.status == OrderStatus::PartiallyFilled)
                    && ord.remaining_shares > 0
                    && now >= ord.eligible_at
                {
                    total_eligible_shares += ord.remaining_shares;
                    eligible_orders.push_back(ord);
                }
            }
        }

        let epoch: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentEpoch)
            .unwrap_or(0)
            + 1;
        env.storage().instance().set(&DataKey::CurrentEpoch, &epoch);

        if total_eligible_shares == 0 || available_liquidity == 0 || state.total_shares == 0 {
            let snap = EpochSnapshot {
                epoch,
                total_eligible_shares,
                total_filled_shares: 0,
                total_fillable_amount: 0,
                fill_ratio_bps: 0,
            };
            env.storage()
                .persistent()
                .set(&DataKey::EpochSnapshot(epoch), &snap);
            return Ok(snap);
        }

        let full_value_needed = total_eligible_shares * state.total_capital / state.total_shares;
        let epoch_budget = available_liquidity.min(full_value_needed);
        let total_shares_to_fill = if epoch_budget == full_value_needed {
            total_eligible_shares
        } else {
            epoch_budget * state.total_shares / state.total_capital
        };
        let fill_ratio_bps = total_shares_to_fill * BPS_DENOM / total_eligible_shares;

        let mut filled_shares_acc: i128 = 0;
        let mut filled_amount_acc: i128 = 0;
        let mut total_queued: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalQueuedShares)
            .unwrap_or(0);

        let count = eligible_orders.len();
        for idx in 0..count {
            let mut ord = eligible_orders.get(idx).unwrap();
            let shares_fill = if idx + 1 == count {
                (total_shares_to_fill - filled_shares_acc).min(ord.remaining_shares)
            } else {
                (total_shares_to_fill * ord.remaining_shares / total_eligible_shares)
                    .min(ord.remaining_shares)
            };
            let amount_fill = if idx + 1 == count {
                epoch_budget - filled_amount_acc
            } else {
                epoch_budget * ord.remaining_shares / total_eligible_shares
            };

            filled_shares_acc += shares_fill;
            filled_amount_acc += amount_fill;

            ord.remaining_shares -= shares_fill;
            ord.claimable_principal += amount_fill;
            if ord.remaining_shares == 0 {
                ord.status = OrderStatus::Filled;
                let open_count: u32 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::OpenOrders(ord.staker.clone()))
                    .unwrap_or(0);
                env.storage().persistent().set(
                    &DataKey::OpenOrders(ord.staker.clone()),
                    &open_count.saturating_sub(1),
                );
            } else if shares_fill > 0 {
                ord.status = OrderStatus::PartiallyFilled;
            }
            env.storage()
                .persistent()
                .set(&DataKey::Order(ord.id), &ord);
        }

        total_queued -= filled_shares_acc;
        state.total_shares -= filled_shares_acc;
        state.total_capital -= filled_amount_acc;
        env.storage()
            .instance()
            .set(&DataKey::TotalQueuedShares, &total_queued);
        env.storage().instance().set(&DataKey::State, &state);

        let snap = EpochSnapshot {
            epoch,
            total_eligible_shares,
            total_filled_shares: filled_shares_acc,
            total_fillable_amount: filled_amount_acc,
            fill_ratio_bps,
        };
        env.storage()
            .persistent()
            .set(&DataKey::EpochSnapshot(epoch), &snap);
        Ok(snap)
    }

    /// Claims filled principal on a withdrawal order, leaving any unfilled shares queued.
    pub fn claim_withdraw(env: Env, staker: Address, order_id: u64) -> Result<i128, BackstopError> {
        staker.require_auth();
        let config = Self::read_config(&env)?;
        let mut order: WithdrawOrder = env
            .storage()
            .persistent()
            .get(&DataKey::Order(order_id))
            .ok_or(BackstopError::OrderNotFound)?;
        if order.staker != staker {
            return Err(BackstopError::Unauthorized);
        }
        if order.claimable_principal <= 0 {
            return Err(BackstopError::NothingToClaim);
        }

        let payout = order.claimable_principal;
        order.claimable_principal = 0;
        order.claimed_principal += payout;
        env.storage()
            .persistent()
            .set(&DataKey::Order(order_id), &order);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &staker,
            &payout,
        );
        Ok(payout)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Shares(staker))
            .unwrap_or(0)
    }

    pub fn total_queued_shares(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalQueuedShares)
            .unwrap_or(0)
    }

    pub fn withdraw_order(env: Env, order_id: u64) -> Option<WithdrawOrder> {
        env.storage().persistent().get(&DataKey::Order(order_id))
    }

    pub fn epoch_snapshot(env: Env, epoch: u64) -> Option<EpochSnapshot> {
        env.storage()
            .persistent()
            .get(&DataKey::EpochSnapshot(epoch))
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    fn read_policy(env: &Env) -> Result<QueuePolicy, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::QueuePolicy)
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    #[test]
    fn withdrawal_queue_fills_pro_rata_respects_solvency_floor_and_loss_freeze() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let s1 = Address::generate(&env);
        let s2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&s1, &10_000);
        asset.mint(&s2, &10_000);
        let tok = token::Client::new(&env, &token_id);

        let cid = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &cid);

        client.initialize(&pool, &admin, &token_id, &5_000);
        // Cooldown = 5 ledgers, min = 100, max open orders = 2, solvency floor = 3_000
        client.set_queue_policy(&5, &100, &2, &3_000, &false);

        client.deposit(&s1, &3_000);
        client.deposit(&s2, &1_000);
        // Total capital = 4_000, solvency floor = 3_000 -> available liquidity = 1_000

        let o1 = client.request_withdraw(&s1, &1_500);
        let o2 = client.request_withdraw(&s2, &500);

        // Invariant: active_shares + total_queued_shares == total_shares (4_000)
        assert_eq!(
            client.shares_of(&s1) + client.shares_of(&s2) + client.total_queued_shares(),
            client.state().total_shares
        );

        // Advance past cooldown (ledger 6): total eligible = 2_000, available = 1_000 (50% pro-rata fill)
        env.ledger().with_mut(|l| l.sequence_number = 6);
        let snap = client.settle_epoch(&admin);
        assert_eq!(snap.fill_ratio_bps, 5_000);
        assert_eq!(snap.total_fillable_amount, 1_000);

        // o1 gets 750 filled, o2 gets 250 filled; cancel after partial fill is blocked
        assert_eq!(
            client.try_cancel_withdraw(&s1, &o1),
            Err(Ok(BackstopError::CannotCancelAfterPartialFill))
        );
        assert_eq!(client.claim_withdraw(&s1, &o1), 750);
        assert_eq!(client.claim_withdraw(&s2, &o2), 250);
        assert_eq!(tok.balance(&s1), 7_750);
        assert_eq!(tok.balance(&s2), 9_250);

        // Loss event freeze blocks epoch settlement
        client.set_queue_policy(&5, &100, &2, &3_000, &true);
        assert_eq!(
            client.try_settle_epoch(&admin),
            Err(Ok(BackstopError::LossEventFreezeActive))
        );
    }
}
