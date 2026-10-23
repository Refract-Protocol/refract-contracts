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
