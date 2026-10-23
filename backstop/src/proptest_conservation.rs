//! Property test: across random sequences of deposits, drawdowns, unstakes
//! and withdrawals, no value is created or lost:
//!
//! `deposited = total_capital + total_drawdowns + withdrawn`
//!
//! and the token balance held by the backstop always equals `total_capital`.

use super::*;
use proptest::prelude::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

#[derive(Clone, Debug)]
enum Op {
    Deposit { who: usize, amount: i128 },
    Draw { amount: i128 },
    Exit { who: usize, bps: u32 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..3, 1i128..1_000_000_000).prop_map(|(who, amount)| Op::Deposit { who, amount }),
        (1i128..2_000_000_000).prop_map(|amount| Op::Draw { amount }),
        (0usize..3, 1u32..=10_000).prop_map(|(who, bps)| Op::Exit { who, bps }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

    #[test]
    fn drawdowns_plus_remaining_stake_equal_deposits(ops in proptest::collection::vec(op(), 1..20)) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let usdc = TokenClient::new(&env, &sac.address());
        let mint = StellarAssetClient::new(&env, &sac.address());
        let pool = Address::generate(&env);
        let id = env.register_contract(None, RefractBackstop);
        let backstop = RefractBackstopClient::new(&env, &id);
        backstop.initialize(&pool, &admin, &sac.address(), &0);
        backstop.set_cooldown(&admin, &0);

        let stakers = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];
        let mut deposited: i128 = 0;
        let mut withdrawn: i128 = 0;
        let mut claim: u64 = 0;

        for op in ops {
            match op {
                Op::Deposit { who, amount } => {
                    mint.mint(&stakers[who], &amount);
                    if backstop.try_deposit(&stakers[who], &amount).is_ok() {
                        deposited += amount;
                    }
                }
                Op::Draw { amount } => {
                    claim += 1;
                    let before = backstop.state();
                    match backstop.try_draw_down(&pool, &claim, &amount) {
                        Ok(Ok(paid)) => prop_assert_eq!(paid, amount),
                        _ => {
                            // A refused draw changes nothing.
                            prop_assert!(amount > before.total_capital);
                            prop_assert_eq!(backstop.state(), before);
                        }
                    }
                }
                Op::Exit { who, bps } => {
                    let active = backstop.get_stake(&stakers[who]).active_shares;
                    let shares = active * bps as i128 / 10_000;
                    if shares > 0 {
                        backstop.request_unstake(&stakers[who], &shares);
                        withdrawn += backstop.withdraw(&stakers[who], &shares);
                    }
                }
            }

            let s = backstop.state();
            prop_assert_eq!(deposited, s.total_capital + s.total_drawdowns + withdrawn);
            prop_assert_eq!(usdc.balance(&id), s.total_capital);
            prop_assert_eq!(usdc.balance(&pool), s.total_drawdowns);
            prop_assert!(s.total_capital >= 0);
            // Attributed impact never exceeds what was actually drawn.
            let attributed: i128 = stakers
                .iter()
                .map(|a| backstop.drawdown_impact_of(a))
                .sum();
            prop_assert!(attributed <= s.total_drawdowns);
        }
    }
}
