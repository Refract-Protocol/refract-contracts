#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
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
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
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
    ExceedsRescuableExcess = 7,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        Ok(())
    }

    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance_internal(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance_internal(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Returns accounted escrow liabilities for `token` (`Balance` for the bounty token, `0` otherwise).
    pub fn accounted_liabilities(env: Env, token: Address) -> Result<i128, BountyError> {
        let bounty_token = Self::token(&env)?;
        let balance = Self::balance_internal(&env)?;
        if token == bounty_token {
            Ok(balance)
        } else {
            Ok(0)
        }
    }

    /// Returns `max(0, token.balance(self) - accounted_liabilities(token))`.
    pub fn rescuable_excess(env: Env, token: Address) -> Result<i128, BountyError> {
        let liabilities = Self::accounted_liabilities(env.clone(), token.clone())?;
        let actual = token::Client::new(&env, &token).balance(&env.current_contract_address());
        Ok((actual - liabilities).max(0))
    }

    /// Governance/timelock-gated rescue entrypoint (`#259`).
    pub fn rescue(
        env: Env,
        caller: Address,
        token: Address,
        to: Address,
        amount: i128,
    ) -> Result<i128, BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let liabilities = Self::accounted_liabilities(env.clone(), token.clone())?;
        let token_client = token::Client::new(&env, &token);
        let pre_balance = token_client.balance(&env.current_contract_address());
        let excess = (pre_balance - liabilities).max(0);
        if amount > excess {
            return Err(BountyError::ExceedsRescuableExcess);
        }

        token_client.transfer(&env.current_contract_address(), &to, &amount);

        let post_balance = token_client.balance(&env.current_contract_address());
        assert!(
            post_balance >= liabilities,
            "post-rescue balance violated bounty escrow liabilities"
        );

        env.events()
            .publish((symbol_short!("rescued"), token, to), (amount, caller));
        Ok(amount)
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance_internal(&env)
    }

    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::governance_internal(&env)
    }

    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn governance_internal(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn balance_internal(env: &Env) -> Result<i128, BountyError> {
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
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn bounty_rescue_protects_escrowed_bounty_pool_and_sweeps_excess() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let stranger = Address::generate(&env);
        let donor = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);

        // Fund 500 USDC through `fund` (accounted liability = 500)
        usdc_sac.mint(&donor, &500);
        client.fund(&donor, &500);

        // Misrouted direct transfer of 120 USDC
        usdc_sac.mint(&contract_id, &120);
        assert_eq!(client.accounted_liabilities(&usdc), 500);
        assert_eq!(client.rescuable_excess(&usdc), 120);

        // Non-governance caller rejected
        assert_eq!(
            client.try_rescue(&stranger, &usdc, &recipient, &120),
            Err(Ok(BountyError::Unauthorized))
        );

        // Over-rescue rejected
        assert_eq!(
            client.try_rescue(&gov, &usdc, &recipient, &121),
            Err(Ok(BountyError::ExceedsRescuableExcess))
        );

        // Exact excess rescued cleanly
        assert_eq!(client.rescue(&gov, &usdc, &recipient, &120), 120);
        assert_eq!(client.rescuable_excess(&usdc), 0);
    }
}
