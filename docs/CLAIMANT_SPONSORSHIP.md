# Claimant sponsorship and fee-bump policy

Bug bounty claimants and grant recipients are one-shot users. They must not
need XLM for fees or reserves before a payout. Sponsorship is limited to those
claim and disbursement entrypoints. Administrative and governance calls are
not fee-bumped.

## Who the sponsor is

The sponsor is the protocol multisig (`multisig/`). No single hot key is the
sponsor. Adding, rotating, or revoking the sponsor goes through the multisig
propose / approve / execute flow, which calls `set_sponsor` or
`revoke_sponsorship` on the bounty escrow and the grants contract.

## What is sponsored

| Item | Sponsored |
| --- | --- |
| Account reserve for a new claimant account | Only when the inner transaction already contains a successful `createAccount` for that claimant. The relay will not add one. |
| Trustline reserve | Only when the inner transaction already contains that trustline operation. |
| Transaction fee | Yes. The sponsor wraps the claimant-signed inner transaction in a fee-bump. |
| Ongoing reserves after the payout | No, unless the reserve-sponsorship operation inside the inner transaction succeeded. A fee-bump does not keep sponsoring reserves. |

## Spend accounting

Each contract keeps a per-epoch counter of sponsored payouts and a ceiling set
by governance. When spend reaches 80% of the ceiling the bounty contract emits
`SPEND` / `CEILING`. A payout that would pass the ceiling fails with
`SponsorCeilingReached` (bounty) or `SponsorCeilingReached` (grants) and moves
no funds. The relay reads the same ceiling from `sponsor-policy.json` and
refuses to submit once the local counter would pass it.

## When the sponsor runs out

"Sponsor runs dry" can mean three different things. The behaviour of each, as
the code stands today:

1. Sponsor ceiling reached (relay). `check_policy` in
   `scripts/fee_bump_relay.py` raises `RelayError("sponsor ceiling reached")`
   when `spent + amount` would pass `ceiling` in `sponsor-policy.json`. The
   relay exits with that error before building or submitting the envelope, and
   `spent` is not updated. Nothing reaches the chain; the claimant's award or
   grant is untouched and can be retried once governance raises the ceiling or
   the epoch resets the policy. A revoked policy fails the same way with
   `RelayError("sponsorship revoked")`.
2. Paying contract short of tokens (on chain). `pay` in `common/src/payout.rs`
   calls `try_transfer`; a token balance error maps to
   `TransferOutcome::InsufficientBalance`. The call does not revert. `pay`
   writes a `PayoutRecord` with status `Failed` and reason
   `InsufficientBalance`, adds `amount` to the token's liability, and emits a
   `pay_fail` event. The host contract keeps its own state update, and the
   record can be re-attempted with `retry` (or reclaimed with `reclaim`). The
   claimant sees a failed payout record, not a transaction error.
3. Escrow top-up from a sponsor budget. `ProgramEscrow::auto_refill` in
   `bounty/src/escrow_refill.rs` returns `EscrowError::InsufficientSponsorFunds`
   and leaves the escrow unchanged when the budget cannot cover the refill.

Known limitations:

- `treasury/src/payments.rs` has no sponsor-balance check; the on-chain
  behaviour above lives in `common/src/payout.rs`.
- `bounty/src/escrow_refill.rs` is not declared as a module anywhere in the
  workspace, so it is not compiled or reachable from a contract today.
- The on-chain errors `SponsorCeilingReached` and `SponsorshipRevoked`, and the
  `set_sponsor` / `revoke_sponsorship` entrypoints, and the `SPEND` / `CEILING`
  events named elsewhere in this document do not exist in the contract code
  (`rg SponsorCeilingReached` only matches this file). The ceiling and
  revocation checks are enforced only by the off-chain relay.

## Revocation

The sponsor calls `revoke_sponsorship`. That sets a flag. It does not move
tokens and does not change awards or grants already credited. A claim or
disburse still in flight then fails with `SponsorshipRevoked`.

## Authorization

Fee-bumping is off-chain. The contract still requires the claimant's
authorization over the destination and the amount:

- Bounty `claim` requires `claimant.require_auth()` and `destination == claimant`.
- Grants `disburse` requires `destination == recipient` and that address to be
  the grant recipient.

The sponsor cannot redirect funds by editing the inner transaction. The relay
refuses a destination, amount, or memo that does not match the claimant's
signed commitment.

## Relay

`scripts/fee_bump_relay.py` builds the fee-bump envelope. It retries when the
sponsor sequence number changed. It does not log claimant keys or the full
signed envelope. Claimants who already paid their own fee are not refunded.

## Claimant steps

1. Governance opens the award (`open_award`) or confirms the grant milestone.
2. The claimant signs an inner transaction whose only payout operation is
   `claim` or `disburse` to their own address, for the opened amount.
3. They send the signed inner transaction and the commitment
   (`destination`, `amount`, `memo`) to the sponsor operator. They do not send
   a secret key.
4. The sponsor submits the fee-bump. The claimant receives the tokens at the
   address they signed.
