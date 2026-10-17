#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env,
};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Whether the bounty escrow is currently paused.
    Paused,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

/// Read-only simulation result returned by [`RefractBountyEscrow::preview_bounty_payout`].
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyPayoutPreview {
    /// Report commitment hash inspected.
    pub report_hash: BytesN<32>,
    /// Whether `report_hash` has already been rewarded on-chain.
    pub already_awarded: bool,
    /// Historical awarded amount if `already_awarded == true`, otherwise the simulated gross payout.
    pub gross_amount: i128,
    /// Protocol fee deducted from the bounty payout (0 by default).
    pub fee: i128,
    /// Net token amount that `award` would transfer to the researcher (0 if duplicate or insufficient escrow).
    pub net_payout: i128,
    /// Escrow balance that would remain after executing the payout.
    pub remaining_balance: i128,
    /// Whether the escrow currently holds enough balance to cover `requested_amount`.
    pub sufficient_balance: bool,
    /// Whether the contract is currently paused.
    pub paused: bool,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    Paused = 7,
}

/// Governance-controlled bug bounty escrow.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Governance-gated pause toggle. Previews still return figures while flagging `paused = true`.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &paused);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if Self::is_paused(&env) {
            return Err(BountyError::Paused);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; shares [`Self::compute_bounty_payout`] with [`Self::preview_bounty_payout`].
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if Self::is_paused(&env) {
            return Err(BountyError::Paused);
        }
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let preview = Self::compute_bounty_payout(&env, &report_hash, amount)?;
        if preview.already_awarded {
            return Err(BountyError::DuplicateReport);
        }
        if !preview.sufficient_balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &preview.net_payout,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &preview.remaining_balance);

        let record = AwardRecord {
            researcher,
            amount: preview.net_payout,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Pure, side-effect-free simulation of awarding `requested_amount` for `report_hash`.
    ///
    /// Performs no storage writes, no `require_auth`, and no token transfers.
    pub fn preview_bounty_payout(
        env: Env,
        report_hash: BytesN<32>,
        requested_amount: i128,
    ) -> Result<BountyPayoutPreview, BountyError> {
        Self::compute_bounty_payout(&env, &report_hash, requested_amount)
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    /// Shared internal payout calculation called by both [`Self::award`] and [`Self::preview_bounty_payout`].
    fn compute_bounty_payout(
        env: &Env,
        report_hash: &BytesN<32>,
        requested_amount: i128,
    ) -> Result<BountyPayoutPreview, BountyError> {
        let balance = Self::read_balance(env)?;
        let paused = Self::is_paused(env);
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<DataKey, AwardRecord>(&DataKey::Award(report_hash.clone()))
        {
            return Ok(BountyPayoutPreview {
                report_hash: report_hash.clone(),
                already_awarded: true,
                gross_amount: existing.amount,
                fee: 0,
                net_payout: 0,
                remaining_balance: balance,
                sufficient_balance: false,
                paused,
            });
        }
        if requested_amount <= 0 {
            return Ok(BountyPayoutPreview {
                report_hash: report_hash.clone(),
                already_awarded: false,
                gross_amount: 0,
                fee: 0,
                net_payout: 0,
                remaining_balance: balance,
                sufficient_balance: true,
                paused,
            });
        }
        let sufficient_balance = requested_amount <= balance;
        let net_payout = if sufficient_balance {
            requested_amount
        } else {
            0
        };
        let remaining_balance = if sufficient_balance {
            balance - net_payout
        } else {
            balance
        };
        Ok(BountyPayoutPreview {
            report_hash: report_hash.clone(),
            already_awarded: false,
            gross_amount: requested_amount,
            fee: 0,
            net_payout,
            remaining_balance,
            sufficient_balance,
            paused,
        })
    }

    fn is_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn preview_bounty_payout_matches_award_and_handles_duplicates_and_pause() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &1_000_0000000);

        client.initialize(&governance, &token_id);
        client.fund(&funder, &500_0000000);

        let report_hash = BytesN::from_array(&env, &[0x42; 32]);
        let preview = client.preview_bounty_payout(&report_hash, &200_0000000);
        assert!(!preview.already_awarded);
        assert!(preview.sufficient_balance);
        assert_eq!(preview.net_payout, 200_0000000);
        assert_eq!(preview.remaining_balance, 300_0000000);
        assert!(!preview.paused);

        // Execute award and verify exact parity with preview.
        client.award(&governance, &researcher, &200_0000000, &report_hash);
        assert_eq!(
            token::Client::new(&env, &token_id).balance(&researcher),
            preview.net_payout
        );
        assert_eq!(client.balance(), preview.remaining_balance);

        // Subsequent preview for the same report_hash flags already_awarded and 0 net_payout.
        let dup_preview = client.preview_bounty_payout(&report_hash, &200_0000000);
        assert!(dup_preview.already_awarded);
        assert_eq!(dup_preview.net_payout, 0);
        assert_eq!(dup_preview.gross_amount, 200_0000000);

        // Paused state is surfaced in preview without blocking read queries.
        client.set_paused(&governance, &true);
        let next_hash = BytesN::from_array(&env, &[0x99; 32]);
        let paused_preview = client.preview_bounty_payout(&next_hash, &100_0000000);
        assert!(paused_preview.paused);
        assert_eq!(paused_preview.net_payout, 100_0000000);
    }
}
