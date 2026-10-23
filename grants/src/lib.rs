//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with multi-funder
//! escrow (issue #250).
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. The grant is funded by the treasury and/or external
//! co-funders. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Design notes
//!
//! * `create_grant` and `confirm_milestone` are governance/timelock-gated.
//!   Confirmation is deliberately separate from creation so different reviewers
//!   can confirm delivery of individual milestones.
//! * `claim` is permissionless for the recipient and releases whatever
//!   confirmed-but-unclaimed amount exists (mirrors the vesting-claim pattern).
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order. Out-of-order confirmation
//!   is rejected.
//! * Zero-milestone grants are explicitly rejected at creation.
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.
//!
//! ## Co-funding
//!
//! A grant can have up to [`MAX_FUNDERS`] funders. Each funder has a position
//! ([`FunderPosition`]) recording what they deposited, what is allocated to
//! confirmed-but-unclaimed milestones, what was paid out to the recipient,
//! and what was refunded. The contract keeps one escrow total per asset
//! ([`RefractGrants::escrow_balance`]).
//!
//! * **Deposits.** External funders call [`RefractGrants::co_fund`]. Who may
//!   do so is set by the [`FundingPolicy`]. The deposit is pulled with a token
//!   transfer and only credited if the contract's balance grew by exactly
//!   `amount`. The treasury, which pushes funds with its own `withdraw`, is
//!   credited with [`RefractGrants::credit_treasury_deposit`]: governance
//!   credits tokens that arrived but aren't owned by any position yet.
//! * **Total cap.** Deposits can't exceed the grant's `total_amount`.
//! * **Disbursement.** When a milestone is confirmed, its amount is split
//!   across funders in proportion to their unspent balance (deposited minus
//!   allocated, paid and refunded). Each share is rounded down.
//! * **Remainder rule.** The units lost to rounding (always fewer than the
//!   number of funders with an unspent balance) go one each to funders in
//!   deposit order (the order they first funded the grant), skipping any
//!   funder whose share already equals their unspent balance. So the full
//!   milestone amount is always allocated and nothing is stuck.
//! * **Cancellation and clawback.** [`RefractGrants::cancel_grant`] stops
//!   further milestones; amounts already confirmed stay claimable by the
//!   recipient. [`RefractGrants::clawback_grant`] also revokes confirmed but
//!   unclaimed amounts. Either way each funder is owed exactly their own
//!   unspent balance (plus their allocated balance on clawback). Since the
//!   pro-rata split already happened per milestone, refunds need no further
//!   rounding.
//! * **Refunds are pulled.** Funders withdraw with
//!   [`RefractGrants::withdraw_refund`]. A funder whose token account is
//!   frozen can't receive their refund, but that doesn't block anyone else.
//!   Their refund stays owed until the account is unfrozen.
//! * **Invariant.** For every asset, the escrow total equals the sum over all
//!   positions of `deposited - disbursed - refunded`
//!   ([`RefractGrants::escrow_matches_positions`]).
//!
//! ## Transfers and reentrancy
//!
//! Token movements go through the [`TokenLedger`] trait (SEP-41 `balance` and
//! `transfer`), which the integrating runtime backs with real token
//! contracts. Outgoing transfers happen after the state is updated, and the
//! state change is undone if the transfer fails, matching how a failed
//! Soroban invocation reverts. An incoming deposit is only credited when the
//! balance delta equals `amount`, so a funder token that reenters during
//! `transfer` and makes a second deposit causes the outer deposit to be
//! rejected instead of double-counted.

use std::collections::{BTreeMap, BTreeSet};

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Most funders a single grant may have. Bounds the per-milestone loop.
pub const MAX_FUNDERS: usize = 16;

/// Errors returned by a [`TokenLedger`] transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// The sender's balance is too low.
    InsufficientBalance,
    /// The sender's or receiver's token account is frozen.
    Frozen,
}

/// Minimal SEP-41 token interface used for grant escrow.
pub trait TokenLedger {
    /// Balance of `owner` in `asset`.
    fn balance(&self, asset: Address, owner: Address) -> i128;
    /// Move `amount` of `asset` from `from` to `to`.
    fn transfer(
        &mut self,
        asset: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), TransferError>;
}

/// In-memory [`TokenLedger`] for tests and off-chain simulation.
#[derive(Debug, Clone, Default)]
pub struct MemoryLedger {
    balances: BTreeMap<(Address, Address), i128>,
    frozen: BTreeSet<(Address, Address)>,
}

impl MemoryLedger {
    /// Creates an empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Credits `amount` of `asset` to `owner`.
    pub fn mint(&mut self, asset: Address, owner: Address, amount: i128) {
        *self.balances.entry((asset, owner)).or_insert(0) += amount;
    }

    /// Freezes or unfreezes `owner`'s account for `asset`.
    pub fn set_frozen(&mut self, asset: Address, owner: Address, frozen: bool) {
        if frozen {
            self.frozen.insert((asset, owner));
        } else {
            self.frozen.remove(&(asset, owner));
        }
    }
}

impl TokenLedger for MemoryLedger {
    fn balance(&self, asset: Address, owner: Address) -> i128 {
        self.balances.get(&(asset, owner)).copied().unwrap_or(0)
    }

    fn transfer(
        &mut self,
        asset: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), TransferError> {
        if self.frozen.contains(&(asset, from)) || self.frozen.contains(&(asset, to)) {
            return Err(TransferError::Frozen);
        }
        let from_balance = self.balance(asset, from);
        if from_balance < amount {
            return Err(TransferError::InsufficientBalance);
        }
        self.balances.insert((asset, from), from_balance - amount);
        *self.balances.entry((asset, to)).or_insert(0) += amount;
        Ok(())
    }
}

/// Errors returned by [`RefractGrants`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    /// Caller is not the configured governance/timelock authority.
    Unauthorized,
    /// No grant exists for the supplied id.
    GrantNotFound,
    /// A grant must declare at least one milestone.
    NoMilestones,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount,
    /// The total amount was zero or negative.
    InvalidTotalAmount,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder,
    /// The caller is not the grant recipient.
    NotRecipient,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim,
    /// A deposit amount was zero or negative.
    InvalidAmount,
    /// The funding policy does not allow this funder.
    FunderNotAllowed,
    /// The grant was cancelled or clawed back.
    GrantNotActive,
    /// The deposit would take total deposits above `total_amount`.
    OverFunded,
    /// The grant already has [`MAX_FUNDERS`] funders.
    TooManyFunders,
    /// Funders' unspent balance doesn't cover the milestone.
    InsufficientEscrow,
    /// The token transfer failed.
    TransferFailed(TransferError),
    /// The contract's balance didn't grow by exactly the deposited amount.
    TransferMismatch,
    /// The caller has no refund owed on this grant.
    NothingToRefund,
    /// No treasury address is configured.
    TreasuryNotSet,
    /// Fewer unowned tokens are held than the amount being credited.
    UnaccountedBalanceTooLow,
    /// An amount computation overflowed.
    ArithmeticOverflow,
}

/// Who may call [`RefractGrants::co_fund`]. The treasury may always fund.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundingPolicy {
    /// Any address may co-fund.
    Permissionless,
    /// Only addresses governance has allowlisted may co-fund.
    Allowlist,
}

/// Whether a grant is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    /// Accepting deposits and milestone confirmations.
    Active,
    /// Cancelled or clawed back; funders can withdraw refunds.
    Cancelled,
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// One funder's position in a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunderPosition {
    /// The funder.
    pub funder: Address,
    /// Total deposited.
    pub deposited: i128,
    /// Allocated to confirmed milestones but not yet claimed by the recipient.
    pub allocated: i128,
    /// Paid out to the recipient.
    pub disbursed: i128,
    /// Owed back to the funder after cancellation, not yet withdrawn.
    pub refund_owed: i128,
    /// Refunded to the funder.
    pub refunded: i128,
}

impl FunderPosition {
    fn new(funder: Address) -> Self {
        Self {
            funder,
            deposited: 0,
            allocated: 0,
            disbursed: 0,
            refund_owed: 0,
            refunded: 0,
        }
    }

    /// Deposits not yet allocated, paid out, or set aside for refund.
    pub fn unspent(&self) -> i128 {
        self.deposited - self.allocated - self.disbursed - self.refund_owed - self.refunded
    }

    /// Tokens of this position still held in escrow.
    pub fn held(&self) -> i128 {
        self.deposited - self.disbursed - self.refunded
    }
}

/// A grant approved by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Account that receives released funds.
    pub recipient: Address,
    /// Token the grant is paid in.
    pub asset: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
    /// Funders in deposit order.
    pub funders: Vec<FunderPosition>,
    /// Active or cancelled.
    pub status: GrantStatus,
}

impl Grant {
    /// Sum of all funders' deposits.
    pub fn total_deposited(&self) -> i128 {
        self.funders.iter().map(|f| f.deposited).sum()
    }

    /// Sum of all funders' unspent balances.
    pub fn total_unspent(&self) -> i128 {
        self.funders.iter().map(|f| f.unspent()).sum()
    }
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// This contract's own address on the token ledger.
    contract_address: Address,
    /// Treasury address credited by `credit_treasury_deposit`.
    treasury: Option<Address>,
    /// Who may call `co_fund`.
    funding_policy: FundingPolicy,
    /// Funders allowed under [`FundingPolicy::Allowlist`].
    allowlist: BTreeSet<Address>,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Escrow held per asset across all grants.
    escrow: BTreeMap<Address, i128>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    /// `contract_address` is the contract's own address on the token ledger.
    /// Co-funding starts allowlist-only.
    pub fn new(governance: Address, contract_address: Address) -> Self {
        Self {
            governance,
            contract_address,
            treasury: None,
            funding_policy: FundingPolicy::Allowlist,
            allowlist: BTreeSet::new(),
            next_grant_id: 0,
            grants: BTreeMap::new(),
            escrow: BTreeMap::new(),
        }
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Returns the configured treasury, if any.
    pub fn treasury(&self) -> Option<Address> {
        self.treasury
    }

    /// Returns the co-funding policy.
    pub fn funding_policy(&self) -> FundingPolicy {
        self.funding_policy
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    /// Returns `funder`'s position in `grant_id`, if any.
    pub fn funder_position(&self, grant_id: GrantId, funder: Address) -> Option<&FunderPosition> {
        self.grants
            .get(&grant_id)?
            .funders
            .iter()
            .find(|p| p.funder == funder)
    }

    /// Escrow held for `asset` across all grants.
    pub fn escrow_balance(&self, asset: Address) -> i128 {
        self.escrow.get(&asset).copied().unwrap_or(0)
    }

    /// Checks the escrow invariant: for every asset, the escrow total equals
    /// the sum of `deposited - disbursed - refunded` over all positions.
    pub fn escrow_matches_positions(&self) -> bool {
        let mut expected: BTreeMap<Address, i128> = BTreeMap::new();
        for grant in self.grants.values() {
            let held: i128 = grant.funders.iter().map(|f| f.held()).sum();
            *expected.entry(grant.asset).or_insert(0) += held;
        }
        let assets: BTreeSet<Address> = expected.keys().chain(self.escrow.keys()).copied().collect();
        assets.into_iter().all(|asset| {
            expected.get(&asset).copied().unwrap_or(0) == self.escrow_balance(asset)
        })
    }

    /// Sets the treasury address. Governance-only.
    pub fn set_treasury(&mut self, caller: Address, treasury: Address) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.treasury = Some(treasury);
        Ok(())
    }

    /// Sets who may call `co_fund`. Governance-only.
    pub fn set_funding_policy(
        &mut self,
        caller: Address,
        policy: FundingPolicy,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.funding_policy = policy;
        Ok(())
    }

    /// Adds or removes `funder` from the co-funding allowlist. Governance-only.
    pub fn set_funder_allowed(
        &mut self,
        caller: Address,
        funder: Address,
        allowed: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if allowed {
            self.allowlist.insert(funder);
        } else {
            self.allowlist.remove(&funder);
        }
        Ok(())
    }

    /// Approves a new grant paid in `asset`, with `total_amount` split across
    /// `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        asset: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(*amount).ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                asset,
                total_amount,
                milestones: stored,
                claimable: 0,
                funders: Vec::new(),
                status: GrantStatus::Active,
            },
        );

        Ok(grant_id)
    }

    /// Deposits `amount` of the grant's asset from `funder` into escrow.
    ///
    /// Allowed for the treasury and for funders the [`FundingPolicy`] admits.
    /// The deposit is credited only if the contract's balance grows by exactly
    /// `amount`. A funder may deposit after some milestones are already paid;
    /// their deposit then funds later milestones.
    pub fn co_fund(
        &mut self,
        funder: Address,
        grant_id: GrantId,
        amount: i128,
        ledger: &mut impl TokenLedger,
    ) -> Result<(), GrantsError> {
        if amount <= 0 {
            return Err(GrantsError::InvalidAmount);
        }
        let is_treasury = self.treasury == Some(funder);
        if !is_treasury
            && self.funding_policy == FundingPolicy::Allowlist
            && !self.allowlist.contains(&funder)
        {
            return Err(GrantsError::FunderNotAllowed);
        }
        let asset = self.check_deposit(grant_id, funder, amount)?;

        let before = ledger.balance(asset, self.contract_address);
        ledger
            .transfer(asset, funder, self.contract_address, amount)
            .map_err(GrantsError::TransferFailed)?;
        let after = ledger.balance(asset, self.contract_address);
        if after.checked_sub(before) != Some(amount) {
            // On-chain, returning an error reverts the transfer as well.
            return Err(GrantsError::TransferMismatch);
        }

        self.credit(grant_id, funder, amount)
    }

    /// Credits `amount` already sent by the treasury (for example by a
    /// governance proposal calling the treasury's `withdraw` with this
    /// contract as `to`) to the treasury's position in `grant_id`.
    ///
    /// Governance-only. Only tokens held by the contract that no position
    /// owns yet can be credited, so the same transfer can't be credited
    /// twice.
    pub fn credit_treasury_deposit(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        amount: i128,
        ledger: &impl TokenLedger,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let treasury = self.treasury.ok_or(GrantsError::TreasuryNotSet)?;
        if amount <= 0 {
            return Err(GrantsError::InvalidAmount);
        }
        let asset = self.check_deposit(grant_id, treasury, amount)?;
        let unaccounted = ledger.balance(asset, self.contract_address) - self.escrow_balance(asset);
        if unaccounted < amount {
            return Err(GrantsError::UnaccountedBalanceTooLow);
        }
        self.credit(grant_id, treasury, amount)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order. The amount is drawn from funders pro-rata (see the
    /// crate docs for the remainder rule).
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        if grant.milestones[milestone_index].confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }
        let amount = grant.milestones[milestone_index].amount;

        let shares = pro_rata_shares(&grant.funders, amount)?;
        let claimable = grant
            .claimable
            .checked_add(amount)
            .ok_or(GrantsError::ArithmeticOverflow)?;

        for (position, share) in grant.funders.iter_mut().zip(shares) {
            position.allocated += share;
        }
        grant.milestones[milestone_index].confirmed = true;
        grant.claimable = claimable;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    /// Still works after `cancel_grant` for amounts confirmed before it.
    pub fn claim(
        &mut self,
        recipient: Address,
        grant_id: GrantId,
        ledger: &mut impl TokenLedger,
    ) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        let asset = grant.asset;
        let snapshot = grant.clone();
        let escrow_before = self.escrow_balance(asset);

        // Effects first, then the transfer.
        let grant = self.grants.get_mut(&grant_id).expect("checked above");
        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }
        for position in grant.funders.iter_mut() {
            position.disbursed += position.allocated;
            position.allocated = 0;
        }
        self.escrow.insert(asset, escrow_before - amount);

        if let Err(e) = ledger.transfer(asset, self.contract_address, recipient, amount) {
            self.grants.insert(grant_id, snapshot);
            self.escrow.insert(asset, escrow_before);
            return Err(GrantsError::TransferFailed(e));
        }

        Ok(amount)
    }

    /// Cancels a grant. No further deposits or milestone confirmations;
    /// amounts already confirmed stay claimable by the recipient. Each funder
    /// is owed their unspent balance. Governance-only.
    pub fn cancel_grant(&mut self, caller: Address, grant_id: GrantId) -> Result<(), GrantsError> {
        self.close_grant(caller, grant_id, false)
    }

    /// Like [`RefractGrants::cancel_grant`], but also revokes confirmed but
    /// unclaimed amounts, which are refunded to the funders they were drawn
    /// from. Governance-only.
    pub fn clawback_grant(&mut self, caller: Address, grant_id: GrantId) -> Result<(), GrantsError> {
        self.close_grant(caller, grant_id, true)
    }

    /// Withdraws the refund owed to `funder` on a cancelled grant. Returns the
    /// amount refunded. If the transfer fails (for example the funder's
    /// account is frozen) nothing changes and the refund stays owed.
    pub fn withdraw_refund(
        &mut self,
        funder: Address,
        grant_id: GrantId,
        ledger: &mut impl TokenLedger,
    ) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let asset = grant.asset;
        let position = grant
            .funders
            .iter_mut()
            .find(|p| p.funder == funder)
            .ok_or(GrantsError::NothingToRefund)?;
        let amount = position.refund_owed;
        if amount <= 0 {
            return Err(GrantsError::NothingToRefund);
        }

        position.refund_owed = 0;
        position.refunded += amount;
        let escrow_before = self.escrow_balance(asset);
        self.escrow.insert(asset, escrow_before - amount);

        if let Err(e) = ledger.transfer(asset, self.contract_address, funder, amount) {
            let position = self
                .grants
                .get_mut(&grant_id)
                .and_then(|g| g.funders.iter_mut().find(|p| p.funder == funder))
                .expect("position exists");
            position.refund_owed = amount;
            position.refunded -= amount;
            self.escrow.insert(asset, escrow_before);
            return Err(GrantsError::TransferFailed(e));
        }
        Ok(amount)
    }

    fn close_grant(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        clawback: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        grant.status = GrantStatus::Cancelled;
        for position in grant.funders.iter_mut() {
            let mut owed = position.unspent();
            if clawback {
                owed += position.allocated;
                position.allocated = 0;
            }
            position.refund_owed += owed;
        }
        if clawback {
            grant.claimable = 0;
        }
        Ok(())
    }

    /// Validates a deposit and returns the grant's asset.
    fn check_deposit(
        &self,
        grant_id: GrantId,
        funder: Address,
        amount: i128,
    ) -> Result<Address, GrantsError> {
        let grant = self.grants.get(&grant_id).ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        let new_total = grant
            .total_deposited()
            .checked_add(amount)
            .ok_or(GrantsError::ArithmeticOverflow)?;
        if new_total > grant.total_amount {
            return Err(GrantsError::OverFunded);
        }
        let is_new = !grant.funders.iter().any(|p| p.funder == funder);
        if is_new && grant.funders.len() >= MAX_FUNDERS {
            return Err(GrantsError::TooManyFunders);
        }
        Ok(grant.asset)
    }

    /// Records a validated deposit.
    fn credit(&mut self, grant_id: GrantId, funder: Address, amount: i128) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let asset = grant.asset;
        let index = match grant.funders.iter().position(|p| p.funder == funder) {
            Some(i) => i,
            None => {
                grant.funders.push(FunderPosition::new(funder));
                grant.funders.len() - 1
            }
        };
        grant.funders[index].deposited += amount;
        let escrow = self
            .escrow_balance(asset)
            .checked_add(amount)
            .ok_or(GrantsError::ArithmeticOverflow)?;
        self.escrow.insert(asset, escrow);
        Ok(())
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

/// Splits `amount` across `funders` in proportion to their unspent balance.
///
/// Each share is `floor(amount * unspent_i / total_unspent)`. The leftover
/// units go one each to funders in order, skipping any funder whose share
/// already equals their unspent balance. Returns one share per funder, in
/// the same order. The shares sum to `amount` and no share exceeds that
/// funder's unspent balance.
pub fn pro_rata_shares(funders: &[FunderPosition], amount: i128) -> Result<Vec<i128>, GrantsError> {
    let total: i128 = funders.iter().map(|f| f.unspent()).sum();
    if amount <= 0 || total < amount {
        return Err(GrantsError::InsufficientEscrow);
    }
    let mut shares = Vec::with_capacity(funders.len());
    for f in funders {
        let share = amount
            .checked_mul(f.unspent())
            .ok_or(GrantsError::ArithmeticOverflow)?
            / total;
        shares.push(share);
    }
    let mut remainder = amount - shares.iter().sum::<i128>();
    for (share, f) in shares.iter_mut().zip(funders) {
        if remainder == 0 {
            break;
        }
        if *share < f.unspent() {
            *share += 1;
            remainder -= 1;
        }
    }
    if remainder != 0 {
        // Unreachable: the leftover is always smaller than the number of
        // funders that still have room.
        return Err(GrantsError::InsufficientEscrow);
    }
    Ok(shares)
}

#[cfg(test)]
mod test;
