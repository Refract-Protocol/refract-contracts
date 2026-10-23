use super::schema::{LegacyInputFile, MigrationError};
use super::state_provider::StateProvider;
use super::timelock_proposal::{GovernanceProposal, TimelockBatchProposal, TimelockOperation};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single concrete contract invocation planned for execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContractInvocation {
    pub step: usize,
    pub program: String,
    pub legacy_id: Option<String>,
    pub target_contract: String,
    pub target_address: String,
    pub function: String,
    pub args: serde_json::Value,
    pub description: String,
}

/// Comprehensive summary of records and liabilities by program and asset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ProgramSummary {
    pub total_records: usize,
    pub already_migrated_count: usize,
    pub planned_actions_count: usize,
    pub bounty_records_total: usize,
    pub bounty_records_migrated: usize,
    pub bounty_records_pending: usize,
    pub grant_records_total: usize,
    pub grant_records_migrated: usize,
    pub grant_records_pending: usize,
    /// Program -> (Asset -> Total liability in smallest units)
    pub liabilities_by_program: BTreeMap<String, BTreeMap<String, i128>>,
    /// Program -> (Asset -> Amount already paid off-chain)
    pub historical_paid_by_program: BTreeMap<String, BTreeMap<String, i128>>,
    /// Program -> (Asset -> Outstanding obligation needing on-chain funding)
    pub outstanding_obligations_by_program: BTreeMap<String, BTreeMap<String, i128>>,
    /// Asset -> Total outstanding liabilities across all programs
    pub total_outstanding_by_asset: BTreeMap<String, i128>,
    /// Asset -> Required treasury disbursement to fund escrows
    pub required_treasury_funding: BTreeMap<String, i128>,
}

/// Complete planned migration output emitted by the generator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationPlan {
    pub version: String,
    pub generated_at: u64,
    pub eta: u64,
    pub actions: Vec<ContractInvocation>,
    pub already_migrated: Vec<String>,
    pub summary: ProgramSummary,
    pub timelock_proposal: TimelockBatchProposal,
    pub governance_proposal: GovernanceProposal,
}

/// Generator options.
#[derive(Debug, Clone)]
pub struct GeneratorConfig {
    pub current_timestamp: u64,
    pub min_delay: u64,
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            current_timestamp: 1_700_000_000,
            min_delay: 86_400, // 24h timelock delay
        }
    }
}

/// Generates a deterministic, idempotent migration plan from input records and state.
pub fn generate_migration_plan(
    input: &LegacyInputFile,
    state: &dyn StateProvider,
    config: &GeneratorConfig,
) -> Result<MigrationPlan, MigrationError> {
    // 1. Validate the input file first
    input.validate()?;

    let mut summary = ProgramSummary::default();
    summary.total_records = input.bounties.len() + input.grants.len();
    summary.bounty_records_total = input.bounties.len();
    summary.grant_records_total = input.grants.len();

    let mut already_migrated = Vec::new();

    // Map asset symbol to address
    let asset_addresses: BTreeMap<String, String> = input
        .assets
        .iter()
        .map(|a| (a.symbol.clone(), a.address.clone()))
        .collect();

    // Track outstanding bounty obligations by asset that need funding into escrow
    let mut pending_bounty_obligations: BTreeMap<String, i128> = BTreeMap::new();
    let mut pending_bounties = Vec::new();

    for bounty in &input.bounties {
        if state.has_legacy_bounty(&bounty.legacy_id) {
            summary.already_migrated_count += 1;
            summary.bounty_records_migrated += 1;
            already_migrated.push(bounty.legacy_id.clone());
            continue;
        }

        summary.bounty_records_pending += 1;
        pending_bounties.push(bounty);

        let prog = "bounty";
        let asset = &bounty.asset;

        *summary
            .liabilities_by_program
            .entry(prog.to_string())
            .or_default()
            .entry(asset.clone())
            .or_default() += bounty.amount;

        if bounty.paid_offchain {
            *summary
                .historical_paid_by_program
                .entry(prog.to_string())
                .or_default()
                .entry(asset.clone())
                .or_default() += bounty.amount;
        } else {
            *summary
                .outstanding_obligations_by_program
                .entry(prog.to_string())
                .or_default()
                .entry(asset.clone())
                .or_default() += bounty.amount;

            *summary
                .total_outstanding_by_asset
                .entry(asset.clone())
                .or_default() += bounty.amount;

            *pending_bounty_obligations.entry(asset.clone()).or_default() += bounty.amount;
        }
    }

    // Process grants
    let mut pending_grants = Vec::new();

    for grant in &input.grants {
        if state.has_legacy_grant(&grant.legacy_id) {
            summary.already_migrated_count += 1;
            summary.grant_records_migrated += 1;
            already_migrated.push(grant.legacy_id.clone());
            continue;
        }

        summary.grant_records_pending += 1;
        pending_grants.push(grant);

        let prog = "grants";
        let asset = &grant.asset;

        let mut grant_claimed: i128 = 0;
        for m in &grant.milestones {
            if m.claimed {
                grant_claimed += m.amount;
            }
        }

        let grant_outstanding = grant.total_amount - grant_claimed;

        *summary
            .liabilities_by_program
            .entry(prog.to_string())
            .or_default()
            .entry(asset.clone())
            .or_default() += grant.total_amount;

        *summary
            .historical_paid_by_program
            .entry(prog.to_string())
            .or_default()
            .entry(asset.clone())
            .or_default() += grant_claimed;

        *summary
            .outstanding_obligations_by_program
            .entry(prog.to_string())
            .or_default()
            .entry(asset.clone())
            .or_default() += grant_outstanding;

        *summary
            .total_outstanding_by_asset
            .entry(asset.clone())
            .or_default() += grant_outstanding;
    }

    // Determine required treasury funding for bounty escrow
    let mut treasury_funding_actions = Vec::new();
    for (asset, needed_amount) in &pending_bounty_obligations {
        let current_escrow = state.get_escrow_balance("bounty_escrow", asset);
        let declared_escrow = input
            .escrow_balances
            .balances
            .get("bounty_escrow")
            .and_then(|m| m.get(asset))
            .copied()
            .unwrap_or(0);
        let escrow_balance = current_escrow.max(declared_escrow);

        if *needed_amount > escrow_balance {
            let shortfall = *needed_amount - escrow_balance;
            summary
                .required_treasury_funding
                .insert(asset.clone(), shortfall);

            let token_addr = asset_addresses.get(asset).cloned().unwrap_or_default();

            treasury_funding_actions.push((
                asset.clone(),
                ContractInvocation {
                    step: 0,
                    program: "treasury".to_string(),
                    legacy_id: None,
                    target_contract: "treasury".to_string(),
                    target_address: input.contracts.treasury.clone(),
                    function: "withdraw".to_string(),
                    args: serde_json::json!({
                        "caller": input.contracts.timelock,
                        "token": token_addr,
                        "amount": shortfall,
                        "to": input.contracts.bounty_escrow,
                    }),
                    description: format!(
                        "Fund bounty escrow with {} {} from treasury for outstanding obligations",
                        shortfall, asset
                    ),
                },
            ));
        }
    }

    // Sort treasury actions deterministically by asset
    treasury_funding_actions.sort_by(|a, b| a.0.cmp(&b.0));

    // Sort pending bounties deterministically by legacy_id
    pending_bounties.sort_by(|a, b| a.legacy_id.cmp(&b.legacy_id));

    let mut bounty_actions = Vec::new();
    for bounty in pending_bounties {
        bounty_actions.push(ContractInvocation {
            step: 0,
            program: "bounty".to_string(),
            legacy_id: Some(bounty.legacy_id.clone()),
            target_contract: "bounty_escrow".to_string(),
            target_address: input.contracts.bounty_escrow.clone(),
            function: "import_legacy_award".to_string(),
            args: serde_json::json!({
                "caller": input.contracts.timelock,
                "legacy_id": bounty.legacy_id,
                "researcher": bounty.researcher,
                "amount": bounty.amount,
                "report_hash": bounty.report_hash,
                "paid_offchain": bounty.paid_offchain,
            }),
            description: format!(
                "Import legacy bounty {} ({} {}, paid_offchain: {})",
                bounty.legacy_id, bounty.amount, bounty.asset, bounty.paid_offchain
            ),
        });
    }

    // Sort pending grants deterministically by legacy_id
    pending_grants.sort_by(|a, b| a.legacy_id.cmp(&b.legacy_id));

    let mut grant_actions = Vec::new();
    for grant in pending_grants {
        let confirmed_count = grant.milestones.iter().filter(|m| m.confirmed).count();
        let claimed_count = grant.milestones.iter().filter(|m| m.claimed).count();
        let milestone_amounts: Vec<i128> = grant.milestones.iter().map(|m| m.amount).collect();

        grant_actions.push(ContractInvocation {
            step: 0,
            program: "grants".to_string(),
            legacy_id: Some(grant.legacy_id.clone()),
            target_contract: "grants".to_string(),
            target_address: input.contracts.grants.clone(),
            function: "import_legacy_grant".to_string(),
            args: serde_json::json!({
                "caller": input.contracts.timelock,
                "legacy_id": grant.legacy_id,
                "recipient": grant.recipient,
                "total_amount": grant.total_amount,
                "milestones": milestone_amounts,
                "confirmed_count": confirmed_count,
                "claimed_count": claimed_count,
            }),
            description: format!(
                "Import legacy grant {} (total: {} {}, {}/{} milestones confirmed, {} claimed)",
                grant.legacy_id,
                grant.total_amount,
                grant.asset,
                confirmed_count,
                grant.milestones.len(),
                claimed_count
            ),
        });
    }

    // Combine actions into a single ordered list:
    // 1. Treasury funding actions first
    // 2. Bounty imports
    // 3. Grant imports
    let mut all_actions = Vec::new();
    let mut step = 1;

    for (_asset, mut action) in treasury_funding_actions {
        action.step = step;
        step += 1;
        all_actions.push(action);
    }

    for mut action in bounty_actions {
        action.step = step;
        step += 1;
        all_actions.push(action);
    }

    for mut action in grant_actions {
        action.step = step;
        step += 1;
        all_actions.push(action);
    }

    summary.planned_actions_count = all_actions.len();

    // Build the TimelockBatchProposal
    let eta = config.current_timestamp + config.min_delay;
    let timelock_ops: Vec<TimelockOperation> = all_actions
        .iter()
        .map(|act| TimelockOperation {
            step: act.step,
            target: act.target_address.clone(),
            function: act.function.clone(),
            args: act.args.clone(),
            eta,
            description: act.description.clone(),
        })
        .collect();

    let timelock_batch = TimelockBatchProposal {
        proposal_type: "legacy_migration_batch".to_string(),
        version: input.schema_version.clone(),
        generated_at: config.current_timestamp,
        eta,
        operations: timelock_ops,
    };

    let governance_proposal = GovernanceProposal {
        title: format!("Legacy Records Migration Batch (v{})", input.schema_version),
        description: format!(
            "Execute import of {} legacy records into on-chain escrow contracts via timelock",
            all_actions.len()
        ),
        target_timelock: input.contracts.timelock.clone(),
        batch_proposal: timelock_batch.clone(),
    };

    Ok(MigrationPlan {
        version: input.schema_version.clone(),
        generated_at: config.current_timestamp,
        eta,
        actions: all_actions,
        already_migrated,
        summary,
        timelock_proposal: timelock_batch,
        governance_proposal,
    })
}
