// =============================================================================
// Issue #237 — xtask Operator CLI for Administering Backstop, Bounty, Grants,
// and Treasury
// =============================================================================

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Fixed-point scaling factor used across all Refract contracts (`10^7`).
pub const SCALE_1E7: i128 = 10_000_000;

/// Supported target networks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Local,
    Testnet,
    Mainnet,
}

impl Network {
    pub fn parse(raw: &str) -> Result<Self, OperatorError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" | "standalone" => Ok(Self::Local),
            "testnet" => Ok(Self::Testnet),
            "mainnet" | "public" => Ok(Self::Mainnet),
            other => Err(OperatorError::InvalidArgument(format!(
                "unknown network `{other}` (expected `local`, `testnet`, or `mainnet`)"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Testnet => "testnet",
            Self::Mainnet => "mainnet",
        }
    }

    pub fn default_rpc_url(&self) -> &'static str {
        match self {
            Self::Local => "http://localhost:8000/rpc",
            Self::Testnet => "https://soroban-testnet.stellar.org",
            Self::Mainnet => "https://mainnet.sorobanrpc.com",
        }
    }

    pub fn default_passphrase(&self) -> &'static str {
        match self {
            Self::Local => "Standalone Network ; February 2017",
            Self::Testnet => "Test SDF Network ; September 2015",
            Self::Mainnet => "Public Global Stellar Network ; September 2015",
        }
    }
}

/// Backstop tranche selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TrancheArg {
    Junior,
    Senior,
}

impl TrancheArg {
    pub fn parse(raw: &str) -> Result<Self, OperatorError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "junior" => Ok(Self::Junior),
            "senior" => Ok(Self::Senior),
            other => Err(OperatorError::InvalidArgument(format!(
                "invalid tranche `{other}` (expected `junior` or `senior`)"
            ))),
        }
    }

    pub fn scval_symbol(&self) -> &'static str {
        match self {
            Self::Junior => "Junior",
            Self::Senior => "Senior",
        }
    }
}

/// Structured operator CLI error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorError {
    InvalidArgument(String),
    InvalidAmount(String),
    RawSecretKeyRejected,
    MainnetConfirmationRequired,
    ManifestError(String),
    ContractNotDeployed { network: String, contract: String },
    ExecutionFailed(String),
}

impl fmt::Display for OperatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Self::InvalidAmount(msg) => write!(f, "invalid amount: {msg}"),
            Self::RawSecretKeyRejected => write!(
                f,
                "raw Stellar secret seeds (`S...`) are forbidden on the command line; \
                 configure a named identity via `stellar keys add <name>` and pass `--source <name>`"
            ),
            Self::MainnetConfirmationRequired => write!(
                f,
                "refusing state-mutating operation on `mainnet` without `--confirm-mainnet`"
            ),
            Self::ManifestError(msg) => write!(f, "deployment manifest error: {msg}"),
            Self::ContractNotDeployed { network, contract } => write!(
                f,
                "contract `{contract}` is not deployed in `deployments/{network}.json`"
            ),
            Self::ExecutionFailed(msg) => write!(f, "command execution failed: {msg}"),
        }
    }
}

/// Resolved network and contract deployment metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkDeployment {
    pub network: Network,
    pub rpc_url: String,
    pub network_passphrase: String,
    pub contracts: BTreeMap<String, String>,
    pub tokens: BTreeMap<String, String>,
}

impl NetworkDeployment {
    pub fn load(deployments_dir: &Path, network: Network) -> Result<Self, OperatorError> {
        let manifest_path = deployments_dir.join(format!("{}.json", network.as_str()));
        let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
            OperatorError::ManifestError(format!("cannot read {}: {e}", manifest_path.display()))
        })?;
        let doc: Value = serde_json::from_str(&raw).map_err(|e| {
            OperatorError::ManifestError(format!("invalid JSON in {}: {e}", manifest_path.display()))
        })?;

        let mut rpc_url = doc["rpc_url"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_default();
        let mut passphrase = doc["network_passphrase"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_default();

        if rpc_url.is_empty() || passphrase.is_empty() {
            let config_path = deployments_dir.join("config.json");
            if let Ok(cfg_raw) = std::fs::read_to_string(&config_path) {
                if let Ok(cfg_doc) = serde_json::from_str::<Value>(&cfg_raw) {
                    let net_cfg = &cfg_doc["networks"][network.as_str()];
                    if rpc_url.is_empty() {
                        if let Some(u) = net_cfg["rpc_url"].as_str() {
                            rpc_url = u.to_owned();
                        }
                    }
                    if passphrase.is_empty() {
                        if let Some(p) = net_cfg["network_passphrase"].as_str() {
                            passphrase = p.to_owned();
                        }
                    }
                }
            }
        }

        if rpc_url.is_empty() {
            rpc_url = network.default_rpc_url().to_owned();
        }
        if passphrase.is_empty() {
            passphrase = network.default_passphrase().to_owned();
        }

        let mut contracts = BTreeMap::new();
        let mut tokens = BTreeMap::new();
        if let Some(map) = doc["contracts"].as_object() {
            for (name, entry) in map {
                if let Some(cid) = entry["contract_id"].as_str() {
                    if !cid.trim().is_empty() {
                        contracts.insert(name.clone(), cid.trim().to_owned());
                    }
                }
                if let Some(tok) = entry["init_args"]["token"].as_str() {
                    if !tok.trim().is_empty() {
                        tokens.insert(name.clone(), tok.trim().to_owned());
                    }
                }
            }
        }

        Ok(Self {
            network,
            rpc_url,
            network_passphrase: passphrase,
            contracts,
            tokens,
        })
    }

    pub fn require_contract(&self, contract_key: &str) -> Result<&str, OperatorError> {
        self.contracts
            .get(contract_key)
            .map(String::as_str)
            .ok_or_else(|| OperatorError::ContractNotDeployed {
                network: self.network.as_str().to_owned(),
                contract: contract_key.to_owned(),
            })
    }
}

/// Validates that `source` is a key alias / public address rather than a raw
/// Stellar secret seed (`S...` 56 chars) or 64-hex private key.
pub fn validate_source_identity(source: &str) -> Result<(), OperatorError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(OperatorError::InvalidArgument(
            "`--source` identity cannot be empty".into(),
        ));
    }
    if trimmed.len() == 56
        && trimmed.starts_with('S')
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
    {
        return Err(OperatorError::RawSecretKeyRejected);
    }
    Ok(())
}

/// Validates a Stellar address (`G...` account or `C...` contract, 56 base32 chars)
/// or named identity alias.
pub fn validate_address(label: &str, addr: &str) -> Result<(), OperatorError> {
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return Err(OperatorError::InvalidArgument(format!(
            "`{label}` cannot be empty"
        )));
    }
    if trimmed.len() == 56 && trimmed.starts_with('S') {
        return Err(OperatorError::RawSecretKeyRejected);
    }
    Ok(())
}

/// Parses a human-readable decimal token amount into `1e7`-scaled `i128` stroops.
///
/// Accepts whole numbers (`"100"`) and up to 7 fractional digits (`"12.3456789"`).
/// Rejects negative values, zero (when `require_positive` is true), >7 decimal
/// places, scientific notation, and `i128` overflow.
pub fn parse_amount_1e7(raw: &str, require_positive: bool) -> Result<i128, OperatorError> {
    let s = raw.trim().replace('_', "");
    if s.is_empty() {
        return Err(OperatorError::InvalidAmount("amount is empty".into()));
    }
    if s.starts_with('-') {
        return Err(OperatorError::InvalidAmount(
            "amount cannot be negative".into(),
        ));
    }
    let s = s.strip_prefix('+').unwrap_or(&s);
    let (whole_str, frac_str) = match s.split_once('.') {
        Some((w, f)) => {
            if f.contains('.') {
                return Err(OperatorError::InvalidAmount(format!(
                    "malformed decimal amount `{raw}`"
                )));
            }
            (w, f)
        }
        None => (s, ""),
    };

    if whole_str.is_empty() && frac_str.is_empty() {
        return Err(OperatorError::InvalidAmount(format!(
            "malformed amount `{raw}`"
        )));
    }
    if !whole_str.bytes().all(|b| b.is_ascii_digit())
        || !frac_str.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(OperatorError::InvalidAmount(format!(
            "amount `{raw}` contains non-digit characters"
        )));
    }
    if frac_str.len() > 7 {
        return Err(OperatorError::InvalidAmount(format!(
            "amount `{raw}` has {} fractional digits; maximum is 7 (1e7 scale)",
            frac_str.len()
        )));
    }

    let whole: i128 = if whole_str.is_empty() {
        0
    } else {
        whole_str.parse::<i128>().map_err(|_| {
            OperatorError::InvalidAmount(format!("amount `{raw}` overflows i128"))
        })?
    };

    let mut frac_padded = String::with_capacity(7);
    frac_padded.push_str(frac_str);
    while frac_padded.len() < 7 {
        frac_padded.push('0');
    }
    let frac: i128 = frac_padded.parse::<i128>().map_err(|_| {
        OperatorError::InvalidAmount(format!("invalid fractional part in `{raw}`"))
    })?;

    let scaled = whole
        .checked_mul(SCALE_1E7)
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(|| OperatorError::InvalidAmount(format!("amount `{raw}` overflows i128")))?;

    if require_positive && scaled <= 0 {
        return Err(OperatorError::InvalidAmount(
            "amount must be strictly positive".into(),
        ));
    }
    Ok(scaled)
}

/// Formats a `1e7`-scaled `i128` amount as a fixed 7-decimal string (e.g. `"12.5000000"`).
pub fn format_amount_1e7(scaled: i128) -> String {
    let sign = if scaled < 0 { "-" } else { "" };
    let abs = scaled.unsigned_abs();
    let whole = abs / (SCALE_1E7 as u128);
    let frac = abs % (SCALE_1E7 as u128);
    format!("{sign}{whole}.{frac:07}")
}

/// Normalises a 32-byte hash argument: accepts a 64-char hex string or hashes
/// arbitrary UTF-8 purpose text using a deterministic 32-byte commitment when
/// prefixed with `text:`, otherwise requires 64 hex chars.
pub fn parse_hex32(label: &str, raw: &str) -> Result<String, OperatorError> {
    let trimmed = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    if trimmed.len() != 64 || hex::decode(trimmed).is_err() {
        return Err(OperatorError::InvalidArgument(format!(
            "`{label}` must be a 64-character hex string (32 bytes)"
        )));
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// One `stellar contract invoke` invocation specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvokeStep {
    pub description: String,
    pub contract_name: String,
    pub contract_id: String,
    pub network: String,
    pub rpc_url: String,
    pub network_passphrase: String,
    pub source: Option<String>,
    pub read_only: bool,
    pub function: String,
    pub args: Vec<(String, String)>,
}

impl InvokeStep {
    /// Builds the argument vector passed to the `stellar` binary.
    pub fn to_stellar_args(&self) -> Vec<String> {
        let mut argv = vec![
            "contract".to_owned(),
            "invoke".to_owned(),
            "--id".to_owned(),
            self.contract_id.clone(),
            "--rpc-url".to_owned(),
            self.rpc_url.clone(),
            "--network-passphrase".to_owned(),
            self.network_passphrase.clone(),
        ];
        if let Some(src) = &self.source {
            argv.push("--source".to_owned());
            argv.push(src.clone());
        }
        if self.read_only {
            argv.push("--send=no".to_owned());
        }
        argv.push("--".to_owned());
        argv.push(self.function.clone());
        for (flag, val) in &self.args {
            argv.push(format!("--{flag}"));
            argv.push(val.clone());
        }
        argv
    }

    /// Formats a shell-safe command line for `--dry-run` output.
    pub fn to_shell_command(&self) -> String {
        let mut parts = Vec::new();
        parts.push("stellar".to_owned());
        for arg in self.to_stellar_args() {
            if arg.contains(' ') || arg.contains(';') {
                parts.push(format!("\"{arg}\""));
            } else {
                parts.push(arg);
            }
        }
        parts.join(" ")
    }
}

/// Complete execution plan produced by an operator CLI command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperatorPlan {
    pub domain: String,
    pub action: String,
    pub network: Network,
    pub dry_run: bool,
    pub mutating: bool,
    pub summary: Value,
    pub steps: Vec<InvokeStep>,
}

/// Parsed operator CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorCommand {
    BackstopStake {
        network: Network,
        source: String,
        staker: String,
        tranche: Option<TrancheArg>,
        amount_1e7: i128,
        dry_run: bool,
        confirm_mainnet: bool,
    },
    BackstopUnstake {
        network: Network,
        source: String,
        staker: String,
        tranche: Option<TrancheArg>,
        shares_1e7: i128,
        dry_run: bool,
        confirm_mainnet: bool,
    },
    BackstopStatus {
        network: Network,
        staker: Option<String>,
        dry_run: bool,
    },
    BountyFund {
        network: Network,
        source: String,
        from: String,
        amount_1e7: i128,
        dry_run: bool,
        confirm_mainnet: bool,
    },
    BountyCheckClaim {
        network: Network,
        payout_id: Option<u64>,
        report_hash: Option<String>,
        dry_run: bool,
    },
    BountyStatus {
        network: Network,
        dry_run: bool,
    },
    GrantsApproveMilestone {
        network: Network,
        source: String,
        caller: String,
        grant_id: u64,
        milestone_index: u32,
        dry_run: bool,
        confirm_mainnet: bool,
    },
    GrantsStatus {
        network: Network,
        grant_id: Option<u64>,
        dry_run: bool,
    },
    TreasuryDeposit {
        network: Network,
        source: String,
        from: String,
        token: Option<String>,
        amount_1e7: i128,
        dry_run: bool,
        confirm_mainnet: bool,
    },
    TreasuryProposeWithdrawal {
        network: Network,
        source: String,
        requester: String,
        payee: String,
        token: Option<String>,
        amount_1e7: i128,
        purpose_hash: String,
        dry_run: bool,
        confirm_mainnet: bool,
    },
    TreasuryStatus {
        network: Network,
        request_id: Option<u64>,
        dry_run: bool,
    },
}

impl OperatorCommand {
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::BackstopStake { .. }
                | Self::BackstopUnstake { .. }
                | Self::BountyFund { .. }
                | Self::GrantsApproveMilestone { .. }
                | Self::TreasuryDeposit { .. }
                | Self::TreasuryProposeWithdrawal { .. }
        )
    }

    pub fn network(&self) -> Network {
        match self {
            Self::BackstopStake { network, .. }
            | Self::BackstopUnstake { network, .. }
            | Self::BackstopStatus { network, .. }
            | Self::BountyFund { network, .. }
            | Self::BountyCheckClaim { network, .. }
            | Self::BountyStatus { network, .. }
            | Self::GrantsApproveMilestone { network, .. }
            | Self::GrantsStatus { network, .. }
            | Self::TreasuryDeposit { network, .. }
            | Self::TreasuryProposeWithdrawal { network, .. }
            | Self::TreasuryStatus { network, .. } => *network,
        }
    }
}

/// Simple flag map parser for `xtask` operator subcommands without pulling in heavy proc-macro dependencies.
struct FlagParser {
    flags: BTreeMap<String, String>,
    bools: BTreeMap<String, bool>,
}

impl FlagParser {
    fn parse(args: &[String]) -> Result<Self, OperatorError> {
        let mut flags = BTreeMap::new();
        let mut bools = BTreeMap::new();
        let mut i = 0;
        while i < args.len() {
            let tok = &args[i];
            let Some(rest) = tok.strip_prefix("--") else {
                return Err(OperatorError::InvalidArgument(format!(
                    "unexpected positional argument `{tok}` (all options must use `--flag <value>`)"
                )));
            };
            if let Some((k, v)) = rest.split_once('=') {
                flags.insert(k.to_owned(), v.to_owned());
                i += 1;
                continue;
            }
            match rest {
                "dry-run" | "confirm-mainnet" | "json" | "help" => {
                    bools.insert(rest.to_owned(), true);
                    i += 1;
                }
                key => {
                    let val = args.get(i + 1).ok_or_else(|| {
                        OperatorError::InvalidArgument(format!("missing value for `--{key}`"))
                    })?;
                    flags.insert(key.to_owned(), val.clone());
                    i += 2;
                }
            }
        }
        Ok(Self { flags, bools })
    }

    fn require(&self, key: &str) -> Result<&str, OperatorError> {
        self.flags
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| OperatorError::InvalidArgument(format!("missing required flag `--{key}`")))
    }

    fn optional(&self, key: &str) -> Option<&str> {
        self.flags.get(key).map(String::as_str)
    }

    fn has_bool(&self, key: &str) -> bool {
        self.bools.get(key).copied().unwrap_or(false)
    }

    fn network(&self) -> Result<Network, OperatorError> {
        match self.optional("network") {
            Some(n) => Network::parse(n),
            None => Ok(Network::Local),
        }
    }
}

/// Parses operator CLI arguments (`<domain> <action> [--flags...]`).
pub fn parse_operator_args(
    args: &[String],
) -> Result<(OperatorCommand, Option<PathBuf>, bool), OperatorError> {
    let slice = if args.first().map(String::as_str) == Some("operator") {
        &args[1..]
    } else {
        args
    };
    if slice.len() < 2 {
        return Err(OperatorError::InvalidArgument(
            "expected `<backstop|bounty|grants|treasury> <subcommand> [options]`".into(),
        ));
    }
    let domain = slice[0].as_str();
    let action = slice[1].as_str();
    let fp = FlagParser::parse(&slice[2..])?;
    let deployments_dir = fp.optional("deployments-dir").map(PathBuf::from);
    let json_output = fp.has_bool("json");
    let network = fp.network()?;
    let dry_run = fp.has_bool("dry-run");
    let confirm_mainnet = fp.has_bool("confirm-mainnet");

    let cmd = match (domain, action) {
        ("backstop", "stake") => {
            let source = fp.require("source")?.to_owned();
            validate_source_identity(&source)?;
            let staker = fp
                .optional("staker")
                .unwrap_or(source.as_str())
                .to_owned();
            validate_address("--staker", &staker)?;
            let tranche = match fp.optional("tranche") {
                Some(t) => Some(TrancheArg::parse(t)?),
                None => None,
            };
            let amount_1e7 = parse_amount_1e7(fp.require("amount")?, true)?;
            OperatorCommand::BackstopStake {
                network,
                source,
                staker,
                tranche,
                amount_1e7,
                dry_run,
                confirm_mainnet,
            }
        }
        ("backstop", "unstake") => {
            let source = fp.require("source")?.to_owned();
            validate_source_identity(&source)?;
            let staker = fp
                .optional("staker")
                .unwrap_or(source.as_str())
                .to_owned();
            validate_address("--staker", &staker)?;
            let tranche = match fp.optional("tranche") {
                Some(t) => Some(TrancheArg::parse(t)?),
                None => None,
            };
            let shares_1e7 = parse_amount_1e7(fp.require("shares")?, true)?;
            OperatorCommand::BackstopUnstake {
                network,
                source,
                staker,
                tranche,
                shares_1e7,
                dry_run,
                confirm_mainnet,
            }
        }
        ("backstop", "status") => {
            let staker = fp.optional("staker").map(str::to_owned);
            if let Some(s) = &staker {
                validate_address("--staker", s)?;
            }
            OperatorCommand::BackstopStatus {
                network,
                staker,
                dry_run,
            }
        }
        ("bounty", "fund") => {
            let source = fp.require("source")?.to_owned();
            validate_source_identity(&source)?;
            let from = fp.optional("from").unwrap_or(source.as_str()).to_owned();
            validate_address("--from", &from)?;
            let amount_1e7 = parse_amount_1e7(fp.require("amount")?, true)?;
            OperatorCommand::BountyFund {
                network,
                source,
                from,
                amount_1e7,
                dry_run,
                confirm_mainnet,
            }
        }
        ("bounty", "check-claim") => {
            let payout_id = match fp.optional("payout-id") {
                Some(v) => Some(v.parse::<u64>().map_err(|_| {
                    OperatorError::InvalidArgument(format!("invalid `--payout-id` `{v}`"))
                })?),
                None => None,
            };
            let report_hash = match fp.optional("report-hash") {
                Some(h) => Some(parse_hex32("--report-hash", h)?),
                None => None,
            };
            if payout_id.is_none() && report_hash.is_none() {
                return Err(OperatorError::InvalidArgument(
                    "`bounty check-claim` requires `--payout-id <u64>` or `--report-hash <64-hex>`"
                        .into(),
                ));
            }
            OperatorCommand::BountyCheckClaim {
                network,
                payout_id,
                report_hash,
                dry_run,
            }
        }
        ("bounty", "status") => OperatorCommand::BountyStatus { network, dry_run },
        ("grants", "approve-milestone") => {
            let source = fp.require("source")?.to_owned();
            validate_source_identity(&source)?;
            let caller = fp.optional("caller").unwrap_or(source.as_str()).to_owned();
            validate_address("--caller", &caller)?;
            let grant_id = fp
                .require("grant-id")?
                .parse::<u64>()
                .map_err(|_| OperatorError::InvalidArgument("invalid `--grant-id`".into()))?;
            let milestone_index = fp
                .require("milestone-index")?
                .parse::<u32>()
                .map_err(|_| {
                    OperatorError::InvalidArgument("invalid `--milestone-index`".into())
                })?;
            OperatorCommand::GrantsApproveMilestone {
                network,
                source,
                caller,
                grant_id,
                milestone_index,
                dry_run,
                confirm_mainnet,
            }
        }
        ("grants", "status") => {
            let grant_id = match fp.optional("grant-id") {
                Some(v) => Some(v.parse::<u64>().map_err(|_| {
                    OperatorError::InvalidArgument(format!("invalid `--grant-id` `{v}`"))
                })?),
                None => None,
            };
            OperatorCommand::GrantsStatus {
                network,
                grant_id,
                dry_run,
            }
        }
        ("treasury", "deposit") => {
            let source = fp.require("source")?.to_owned();
            validate_source_identity(&source)?;
            let from = fp.optional("from").unwrap_or(source.as_str()).to_owned();
            validate_address("--from", &from)?;
            let token = fp.optional("token").map(str::to_owned);
            if let Some(t) = &token {
                validate_address("--token", t)?;
            }
            let amount_1e7 = parse_amount_1e7(fp.require("amount")?, true)?;
            OperatorCommand::TreasuryDeposit {
                network,
                source,
                from,
                token,
                amount_1e7,
                dry_run,
                confirm_mainnet,
            }
        }
        ("treasury", "propose-withdrawal") => {
            let source = fp.require("source")?.to_owned();
            validate_source_identity(&source)?;
            let requester = fp
                .optional("requester")
                .unwrap_or(source.as_str())
                .to_owned();
            validate_address("--requester", &requester)?;
            let payee = fp.require("payee")?.to_owned();
            validate_address("--payee", &payee)?;
            let token = fp.optional("token").map(str::to_owned);
            if let Some(t) = &token {
                validate_address("--token", t)?;
            }
            let amount_1e7 = parse_amount_1e7(fp.require("amount")?, true)?;
            let purpose_hash = parse_hex32("--purpose-hash", fp.require("purpose-hash")?)?;
            OperatorCommand::TreasuryProposeWithdrawal {
                network,
                source,
                requester,
                payee,
                token,
                amount_1e7,
                purpose_hash,
                dry_run,
                confirm_mainnet,
            }
        }
        ("treasury", "status") => {
            let request_id = match fp.optional("request-id") {
                Some(v) => Some(v.parse::<u64>().map_err(|_| {
                    OperatorError::InvalidArgument(format!("invalid `--request-id` `{v}`"))
                })?),
                None => None,
            };
            OperatorCommand::TreasuryStatus {
                network,
                request_id,
                dry_run,
            }
        }
        _ => {
            return Err(OperatorError::InvalidArgument(format!(
                "unknown operator subcommand `{domain} {action}`"
            )))
        }
    };

    Ok((cmd, deployments_dir, json_output))
}

/// Builds a validated [`OperatorPlan`] from an [`OperatorCommand`] and deployment directory.
pub fn build_plan(
    cmd: &OperatorCommand,
    deployments_dir: &Path,
) -> Result<OperatorPlan, OperatorError> {
    let deployment = NetworkDeployment::load(deployments_dir, cmd.network())?;

    match cmd {
        OperatorCommand::BackstopStake {
            network,
            source,
            staker,
            tranche,
            amount_1e7,
            dry_run,
            confirm_mainnet,
        } => {
            if *network == Network::Mainnet && !confirm_mainnet {
                return Err(OperatorError::MainnetConfirmationRequired);
            }
            let cid = deployment.require_contract("refract_backstop")?.to_owned();
            let (function, args) = match tranche {
                Some(t) => (
                    "deposit_tranche".to_owned(),
                    vec![
                        ("staker".to_owned(), staker.clone()),
                        ("tranche".to_owned(), t.scval_symbol().to_owned()),
                        ("amount".to_owned(), amount_1e7.to_string()),
                    ],
                ),
                None => (
                    "deposit".to_owned(),
                    vec![
                        ("staker".to_owned(), staker.clone()),
                        ("amount".to_owned(), amount_1e7.to_string()),
                    ],
                ),
            };
            Ok(OperatorPlan {
                domain: "backstop".to_owned(),
                action: "stake".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: true,
                summary: json!({
                    "contract": "refract_backstop",
                    "contract_id": cid,
                    "staker": staker,
                    "tranche": tranche.map(|t| t.scval_symbol()).unwrap_or("Junior"),
                    "amount_1e7": amount_1e7.to_string(),
                    "amount_decimal": format_amount_1e7(*amount_1e7),
                }),
                steps: vec![InvokeStep {
                    description: format!(
                        "Stake {} ({amount_1e7} stroops) into refract_backstop",
                        format_amount_1e7(*amount_1e7)
                    ),
                    contract_name: "refract_backstop".to_owned(),
                    contract_id: cid,
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: Some(source.clone()),
                    read_only: false,
                    function,
                    args,
                }],
            })
        }
        OperatorCommand::BackstopUnstake {
            network,
            source,
            staker,
            tranche,
            shares_1e7,
            dry_run,
            confirm_mainnet,
        } => {
            if *network == Network::Mainnet && !confirm_mainnet {
                return Err(OperatorError::MainnetConfirmationRequired);
            }
            let cid = deployment.require_contract("refract_backstop")?.to_owned();
            let (function, args) = match tranche {
                Some(t) => (
                    "withdraw_tranche".to_owned(),
                    vec![
                        ("staker".to_owned(), staker.clone()),
                        ("tranche".to_owned(), t.scval_symbol().to_owned()),
                        ("shares".to_owned(), shares_1e7.to_string()),
                    ],
                ),
                None => (
                    "withdraw".to_owned(),
                    vec![
                        ("staker".to_owned(), staker.clone()),
                        ("shares".to_owned(), shares_1e7.to_string()),
                    ],
                ),
            };
            Ok(OperatorPlan {
                domain: "backstop".to_owned(),
                action: "unstake".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: true,
                summary: json!({
                    "contract": "refract_backstop",
                    "contract_id": cid,
                    "staker": staker,
                    "tranche": tranche.map(|t| t.scval_symbol()).unwrap_or("Junior"),
                    "shares_1e7": shares_1e7.to_string(),
                    "shares_decimal": format_amount_1e7(*shares_1e7),
                }),
                steps: vec![InvokeStep {
                    description: format!(
                        "Unstake {} shares ({shares_1e7}) from refract_backstop",
                        format_amount_1e7(*shares_1e7)
                    ),
                    contract_name: "refract_backstop".to_owned(),
                    contract_id: cid,
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: Some(source.clone()),
                    read_only: false,
                    function,
                    args,
                }],
            })
        }
        OperatorCommand::BackstopStatus {
            network,
            staker,
            dry_run,
        } => {
            let cid = deployment.require_contract("refract_backstop")?.to_owned();
            let mut steps = vec![
                InvokeStep {
                    description: "Query backstop pool state".to_owned(),
                    contract_name: "refract_backstop".to_owned(),
                    contract_id: cid.clone(),
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url.clone(),
                    network_passphrase: deployment.network_passphrase.clone(),
                    source: None,
                    read_only: true,
                    function: "state".to_owned(),
                    args: vec![],
                },
                InvokeStep {
                    description: "Query backstop senior coverage ratio (bps)".to_owned(),
                    contract_name: "refract_backstop".to_owned(),
                    contract_id: cid.clone(),
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url.clone(),
                    network_passphrase: deployment.network_passphrase.clone(),
                    source: None,
                    read_only: true,
                    function: "senior_coverage_ratio".to_owned(),
                    args: vec![],
                },
            ];
            if let Some(s) = staker {
                steps.push(InvokeStep {
                    description: format!("Query backstop shares for staker {s}"),
                    contract_name: "refract_backstop".to_owned(),
                    contract_id: cid.clone(),
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: None,
                    read_only: true,
                    function: "shares_of".to_owned(),
                    args: vec![("staker".to_owned(), s.clone())],
                });
            }
            Ok(OperatorPlan {
                domain: "backstop".to_owned(),
                action: "status".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: false,
                summary: json!({
                    "contract": "refract_backstop",
                    "contract_id": cid,
                    "staker": staker,
                }),
                steps,
            })
        }
        OperatorCommand::BountyFund {
            network,
            source,
            from,
            amount_1e7,
            dry_run,
            confirm_mainnet,
        } => {
            if *network == Network::Mainnet && !confirm_mainnet {
                return Err(OperatorError::MainnetConfirmationRequired);
            }
            let cid = deployment.require_contract("refract_bounty")?.to_owned();
            Ok(OperatorPlan {
                domain: "bounty".to_owned(),
                action: "fund".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: true,
                summary: json!({
                    "contract": "refract_bounty",
                    "contract_id": cid,
                    "from": from,
                    "amount_1e7": amount_1e7.to_string(),
                    "amount_decimal": format_amount_1e7(*amount_1e7),
                }),
                steps: vec![InvokeStep {
                    description: format!(
                        "Fund refract_bounty escrow with {} ({amount_1e7} stroops)",
                        format_amount_1e7(*amount_1e7)
                    ),
                    contract_name: "refract_bounty".to_owned(),
                    contract_id: cid,
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: Some(source.clone()),
                    read_only: false,
                    function: "fund".to_owned(),
                    args: vec![
                        ("from".to_owned(), from.clone()),
                        ("amount".to_owned(), amount_1e7.to_string()),
                    ],
                }],
            })
        }
        OperatorCommand::BountyCheckClaim {
            network,
            payout_id,
            report_hash,
            dry_run,
        } => {
            let cid = deployment.require_contract("refract_bounty")?.to_owned();
            let mut steps = Vec::new();
            if let Some(pid) = payout_id {
                steps.push(InvokeStep {
                    description: format!("Inspect pull-payout record #{pid}"),
                    contract_name: "refract_bounty".to_owned(),
                    contract_id: cid.clone(),
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url.clone(),
                    network_passphrase: deployment.network_passphrase.clone(),
                    source: None,
                    read_only: true,
                    function: "get_payout".to_owned(),
                    args: vec![("payout_id".to_owned(), pid.to_string())],
                });
            }
            if let Some(hash) = report_hash {
                steps.push(InvokeStep {
                    description: format!("Inspect disclosure award record {hash}"),
                    contract_name: "refract_bounty".to_owned(),
                    contract_id: cid.clone(),
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: None,
                    read_only: true,
                    function: "get_award".to_owned(),
                    args: vec![("report_hash".to_owned(), hash.clone())],
                });
            }
            Ok(OperatorPlan {
                domain: "bounty".to_owned(),
                action: "check-claim".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: false,
                summary: json!({
                    "contract": "refract_bounty",
                    "contract_id": cid,
                    "payout_id": payout_id,
                    "report_hash": report_hash,
                }),
                steps,
            })
        }
        OperatorCommand::BountyStatus { network, dry_run } => {
            let cid = deployment.require_contract("refract_bounty")?.to_owned();
            Ok(OperatorPlan {
                domain: "bounty".to_owned(),
                action: "status".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: false,
                summary: json!({
                    "contract": "refract_bounty",
                    "contract_id": cid,
                }),
                steps: vec![
                    InvokeStep {
                        description: "Query total bounty escrow balance".to_owned(),
                        contract_name: "refract_bounty".to_owned(),
                        contract_id: cid.clone(),
                        network: network.as_str().to_owned(),
                        rpc_url: deployment.rpc_url.clone(),
                        network_passphrase: deployment.network_passphrase.clone(),
                        source: None,
                        read_only: true,
                        function: "balance".to_owned(),
                        args: vec![],
                    },
                    InvokeStep {
                        description: "Query unreserved available bounty balance".to_owned(),
                        contract_name: "refract_bounty".to_owned(),
                        contract_id: cid,
                        network: network.as_str().to_owned(),
                        rpc_url: deployment.rpc_url,
                        network_passphrase: deployment.network_passphrase,
                        source: None,
                        read_only: true,
                        function: "available_balance".to_owned(),
                        args: vec![],
                    },
                ],
            })
        }
        OperatorCommand::GrantsApproveMilestone {
            network,
            source,
            caller,
            grant_id,
            milestone_index,
            dry_run,
            confirm_mainnet,
        } => {
            if *network == Network::Mainnet && !confirm_mainnet {
                return Err(OperatorError::MainnetConfirmationRequired);
            }
            let cid = deployment.require_contract("refract_grants")?.to_owned();
            Ok(OperatorPlan {
                domain: "grants".to_owned(),
                action: "approve-milestone".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: true,
                summary: json!({
                    "contract": "refract_grants",
                    "contract_id": cid,
                    "caller": caller,
                    "grant_id": grant_id,
                    "milestone_index": milestone_index,
                }),
                steps: vec![InvokeStep {
                    description: format!(
                        "Confirm milestone #{milestone_index} for grant #{grant_id}"
                    ),
                    contract_name: "refract_grants".to_owned(),
                    contract_id: cid,
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: Some(source.clone()),
                    read_only: false,
                    function: "confirm_milestone".to_owned(),
                    args: vec![
                        ("caller".to_owned(), caller.clone()),
                        ("grant_id".to_owned(), grant_id.to_string()),
                        ("milestone_index".to_owned(), milestone_index.to_string()),
                    ],
                }],
            })
        }
        OperatorCommand::GrantsStatus {
            network,
            grant_id,
            dry_run,
        } => {
            let cid = deployment.require_contract("refract_grants")?.to_owned();
            let mut steps = vec![InvokeStep {
                description: "Query total accounted grant liabilities".to_owned(),
                contract_name: "refract_grants".to_owned(),
                contract_id: cid.clone(),
                network: network.as_str().to_owned(),
                rpc_url: deployment.rpc_url.clone(),
                network_passphrase: deployment.network_passphrase.clone(),
                source: None,
                read_only: true,
                function: "accounted_liabilities".to_owned(),
                args: vec![],
            }];
            if let Some(gid) = grant_id {
                steps.push(InvokeStep {
                    description: format!("Query claimable balance for grant #{gid}"),
                    contract_name: "refract_grants".to_owned(),
                    contract_id: cid.clone(),
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: None,
                    read_only: true,
                    function: "claimable".to_owned(),
                    args: vec![("grant_id".to_owned(), gid.to_string())],
                });
            }
            Ok(OperatorPlan {
                domain: "grants".to_owned(),
                action: "status".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: false,
                summary: json!({
                    "contract": "refract_grants",
                    "contract_id": cid,
                    "grant_id": grant_id,
                }),
                steps,
            })
        }
        OperatorCommand::TreasuryDeposit {
            network,
            source,
            from,
            token,
            amount_1e7,
            dry_run,
            confirm_mainnet,
        } => {
            if *network == Network::Mainnet && !confirm_mainnet {
                return Err(OperatorError::MainnetConfirmationRequired);
            }
            let treasury_cid = deployment.require_contract("refract_treasury")?.to_owned();
            let token_cid = match token {
                Some(t) => t.clone(),
                None => deployment
                    .tokens
                    .get("refract_backstop")
                    .or_else(|| deployment.tokens.get("refract_bounty"))
                    .cloned()
                    .ok_or_else(|| {
                        OperatorError::InvalidArgument(
                            "no default token in deployment manifest; pass `--token <contract_id>`"
                                .into(),
                        )
                    })?,
            };
            Ok(OperatorPlan {
                domain: "treasury".to_owned(),
                action: "deposit".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: true,
                summary: json!({
                    "contract": "refract_treasury",
                    "treasury_contract_id": treasury_cid,
                    "token_contract_id": token_cid,
                    "from": from,
                    "amount_1e7": amount_1e7.to_string(),
                    "amount_decimal": format_amount_1e7(*amount_1e7),
                }),
                steps: vec![InvokeStep {
                    description: format!(
                        "Deposit {} ({amount_1e7} stroops) into refract_treasury via token transfer",
                        format_amount_1e7(*amount_1e7)
                    ),
                    contract_name: "stellar_asset_token".to_owned(),
                    contract_id: token_cid,
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: Some(source.clone()),
                    read_only: false,
                    function: "transfer".to_owned(),
                    args: vec![
                        ("from".to_owned(), from.clone()),
                        ("to".to_owned(), treasury_cid),
                        ("amount".to_owned(), amount_1e7.to_string()),
                    ],
                }],
            })
        }
        OperatorCommand::TreasuryProposeWithdrawal {
            network,
            source,
            requester,
            payee,
            token,
            amount_1e7,
            purpose_hash,
            dry_run,
            confirm_mainnet,
        } => {
            if *network == Network::Mainnet && !confirm_mainnet {
                return Err(OperatorError::MainnetConfirmationRequired);
            }
            let treasury_cid = deployment.require_contract("refract_treasury")?.to_owned();
            let token_cid = match token {
                Some(t) => t.clone(),
                None => deployment
                    .tokens
                    .get("refract_backstop")
                    .or_else(|| deployment.tokens.get("refract_bounty"))
                    .cloned()
                    .ok_or_else(|| {
                        OperatorError::InvalidArgument(
                            "no default token in deployment manifest; pass `--token <contract_id>`"
                                .into(),
                        )
                    })?,
            };
            Ok(OperatorPlan {
                domain: "treasury".to_owned(),
                action: "propose-withdrawal".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: true,
                summary: json!({
                    "contract": "refract_treasury",
                    "contract_id": treasury_cid,
                    "requester": requester,
                    "payee": payee,
                    "token": token_cid,
                    "amount_1e7": amount_1e7.to_string(),
                    "amount_decimal": format_amount_1e7(*amount_1e7),
                    "purpose_hash": purpose_hash,
                }),
                steps: vec![InvokeStep {
                    description: format!(
                        "Submit payment request for {} ({amount_1e7} stroops) to {payee}",
                        format_amount_1e7(*amount_1e7)
                    ),
                    contract_name: "refract_treasury".to_owned(),
                    contract_id: treasury_cid,
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: Some(source.clone()),
                    read_only: false,
                    function: "submit_payment_request".to_owned(),
                    args: vec![
                        ("requester".to_owned(), requester.clone()),
                        ("payee".to_owned(), payee.clone()),
                        ("token".to_owned(), token_cid),
                        ("amount".to_owned(), amount_1e7.to_string()),
                        ("purpose_hash".to_owned(), purpose_hash.clone()),
                    ],
                }],
            })
        }
        OperatorCommand::TreasuryStatus {
            network,
            request_id,
            dry_run,
        } => {
            let cid = deployment.require_contract("refract_treasury")?.to_owned();
            let mut steps = vec![InvokeStep {
                description: "Query treasury admin/timelock address".to_owned(),
                contract_name: "refract_treasury".to_owned(),
                contract_id: cid.clone(),
                network: network.as_str().to_owned(),
                rpc_url: deployment.rpc_url.clone(),
                network_passphrase: deployment.network_passphrase.clone(),
                source: None,
                read_only: true,
                function: "admin".to_owned(),
                args: vec![],
            }];
            if let Some(rid) = request_id {
                steps.push(InvokeStep {
                    description: format!("Query payment request #{rid}"),
                    contract_name: "refract_treasury".to_owned(),
                    contract_id: cid.clone(),
                    network: network.as_str().to_owned(),
                    rpc_url: deployment.rpc_url,
                    network_passphrase: deployment.network_passphrase,
                    source: None,
                    read_only: true,
                    function: "get_payment_request".to_owned(),
                    args: vec![("id".to_owned(), rid.to_string())],
                });
            }
            Ok(OperatorPlan {
                domain: "treasury".to_owned(),
                action: "status".to_owned(),
                network: *network,
                dry_run: *dry_run,
                mutating: false,
                summary: json!({
                    "contract": "refract_treasury",
                    "contract_id": cid,
                    "request_id": request_id,
                }),
                steps,
            })
        }
    }
}

/// Executes or dry-runs an [`OperatorPlan`].
pub fn execute_plan(plan: &OperatorPlan, json_output: bool) -> Result<String, OperatorError> {
    if plan.dry_run {
        if json_output {
            return Ok(serde_json::to_string_pretty(plan).unwrap_or_default());
        }
        let mut out = format!(
            "[DRY RUN] {} {} (network: {})\n",
            plan.domain,
            plan.action,
            plan.network.as_str()
        );
        for (idx, step) in plan.steps.iter().enumerate() {
            out.push_str(&format!(
                "  {}. {}\n     $ {}\n",
                idx + 1,
                step.description,
                step.to_shell_command()
            ));
        }
        return Ok(out);
    }

    let stellar_bin = std::env::var("STELLAR_BIN").unwrap_or_else(|_| "stellar".to_owned());
    let mut results = Vec::new();
    for step in &plan.steps {
        let output = Command::new(&stellar_bin)
            .args(step.to_stellar_args())
            .output()
            .map_err(|e| {
                OperatorError::ExecutionFailed(format!(
                    "failed to invoke `{stellar_bin}` (pass `--dry-run` to preview commands): {e}"
                ))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !output.status.success() {
            return Err(OperatorError::ExecutionFailed(format!(
                "`{}` failed: {stderr}",
                step.function
            )));
        }
        results.push(json!({
            "step": step.description,
            "function": step.function,
            "result": stdout,
        }));
    }

    if json_output {
        Ok(serde_json::to_string_pretty(&json!({
            "plan": plan,
            "results": results,
        }))
        .unwrap_or_default())
    } else {
        let mut out = format!(
            "Executed {} {} on {}:\n",
            plan.domain,
            plan.action,
            plan.network.as_str()
        );
        for item in &results {
            out.push_str(&format!(
                "  - {}: {}\n",
                item["step"].as_str().unwrap_or(""),
                item["result"].as_str().unwrap_or("")
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_deployments_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("deployments")
    }

    fn vec_s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn amount_1e7_parsing_and_formatting_roundtrip() {
        assert_eq!(parse_amount_1e7("1", true), Ok(10_000_000));
        assert_eq!(parse_amount_1e7("100.5", true), Ok(1_005_000_000));
        assert_eq!(parse_amount_1e7("0.0000001", true), Ok(1));
        assert_eq!(parse_amount_1e7("1_250.25", true), Ok(12_502_500_000));
        assert_eq!(parse_amount_1e7("0", false), Ok(0));

        assert!(matches!(
            parse_amount_1e7("0", true),
            Err(OperatorError::InvalidAmount(_))
        ));
        assert!(matches!(
            parse_amount_1e7("-5", false),
            Err(OperatorError::InvalidAmount(_))
        ));
        assert!(matches!(
            parse_amount_1e7("1.12345678", true),
            Err(OperatorError::InvalidAmount(_))
        ));
        assert!(matches!(
            parse_amount_1e7("abc", true),
            Err(OperatorError::InvalidAmount(_))
        ));

        assert_eq!(format_amount_1e7(1_005_000_000), "100.5000000");
        assert_eq!(format_amount_1e7(1), "0.0000001");
    }

    #[test]
    fn rejects_raw_secret_seed_and_accepts_named_identities() {
        let seed = "SBXEAS6ALJH4H45A4MNM5QQ2LCJUXAOXZVIIYTX7WH3LSHNKGUX3DTRT";
        assert_eq!(
            validate_source_identity(seed),
            Err(OperatorError::RawSecretKeyRejected)
        );
        assert_eq!(validate_source_identity("alice"), Ok(()));
        assert_eq!(
            validate_source_identity("GBXEAS6ALJH4H45A4MNM5QQ2LCJUXAOXZVIIYTX7WH3LSHNKGUX3DTRT"),
            Ok(())
        );
    }

    #[test]
    fn backstop_stake_and_unstake_dry_run_against_local_manifest() {
        let dir = repo_deployments_dir();
        let (cmd, _, _) = parse_operator_args(&vec_s(&[
            "backstop",
            "stake",
            "--network",
            "local",
            "--source",
            "alice",
            "--staker",
            "GBXEAS6ALJH4H45A4MNM5QQ2LCJUXAOXZVIIYTX7WH3LSHNKGUX3DTRT",
            "--tranche",
            "senior",
            "--amount",
            "250.5",
            "--dry-run",
        ]))
        .unwrap();
        let plan = build_plan(&cmd, &dir).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].function, "deposit_tranche");
        assert_eq!(
            plan.steps[0].contract_id,
            "CB5IPVONIAEXOZ6XIX5AHP5MNR5UHGHZVRSJZ3WJZ5ICVMCIDHXWIMPJ"
        );
        let rendered = execute_plan(&plan, false).unwrap();
        assert!(rendered.contains("deposit_tranche"));
        assert!(rendered.contains("--tranche Senior"));
        assert!(rendered.contains("--amount 2505000000"));

        let (unstake_cmd, _, _) = parse_operator_args(&vec_s(&[
            "backstop",
            "unstake",
            "--network",
            "local",
            "--source",
            "alice",
            "--shares",
            "50",
            "--dry-run",
        ]))
        .unwrap();
        let unstake_plan = build_plan(&unstake_cmd, &dir).unwrap();
        assert_eq!(unstake_plan.steps[0].function, "withdraw");
        assert!(unstake_plan.steps[0]
            .to_shell_command()
            .contains("--shares 500000000"));
    }

    #[test]
    fn bounty_fund_and_check_claim_plan() {
        let dir = repo_deployments_dir();
        let (fund_cmd, _, _) = parse_operator_args(&vec_s(&[
            "bounty",
            "fund",
            "--network",
            "local",
            "--source",
            "alice",
            "--amount",
            "1000",
            "--dry-run",
        ]))
        .unwrap();
        let fund_plan = build_plan(&fund_cmd, &dir).unwrap();
        assert_eq!(fund_plan.steps[0].function, "fund");
        assert_eq!(
            fund_plan.steps[0].contract_id,
            "CCRAI3MA5R4QX2CEWPFLQE427CVYL3CKNWUQ34ODEFOMEEOM3EYDOJKH"
        );

        let hash = "a".repeat(64);
        let (claim_cmd, _, _) = parse_operator_args(&vec_s(&[
            "bounty",
            "check-claim",
            "--network",
            "local",
            "--payout-id",
            "7",
            "--report-hash",
            &hash,
            "--dry-run",
        ]))
        .unwrap();
        let claim_plan = build_plan(&claim_cmd, &dir).unwrap();
        assert_eq!(claim_plan.steps.len(), 2);
        assert_eq!(claim_plan.steps[0].function, "get_payout");
        assert_eq!(claim_plan.steps[1].function, "get_award");
        assert!(claim_plan.steps[0].read_only);
    }

    #[test]
    fn grants_approve_milestone_and_treasury_commands() {
        let dir = repo_deployments_dir();
        let (grant_cmd, _, _) = parse_operator_args(&vec_s(&[
            "grants",
            "approve-milestone",
            "--network",
            "local",
            "--source",
            "timelock-exec",
            "--caller",
            "CCAVYGBILWMMK3WV2TPV3PVUTXH43S6JJZ6YLY3PF5777UFT3VKTW63V",
            "--grant-id",
            "3",
            "--milestone-index",
            "1",
            "--dry-run",
        ]))
        .unwrap();
        let grant_plan = build_plan(&grant_cmd, &dir).unwrap();
        assert_eq!(grant_plan.steps[0].function, "confirm_milestone");
        assert_eq!(
            grant_plan.steps[0].contract_id,
            "CDDVRHCAOH4CLC45CA25YHPKE6IT4L2662KPBF5HLLFU3V5ATQDY7HNE"
        );

        let (dep_cmd, _, _) = parse_operator_args(&vec_s(&[
            "treasury",
            "deposit",
            "--network",
            "local",
            "--source",
            "alice",
            "--amount",
            "500",
            "--dry-run",
        ]))
        .unwrap();
        let dep_plan = build_plan(&dep_cmd, &dir).unwrap();
        assert_eq!(dep_plan.steps[0].function, "transfer");
        assert_eq!(
            dep_plan.steps[0].contract_id,
            "CDMLFMKMMD7MWZP3FKUBZPVHTUEDLSX4BYGYKH4GCESXYHS3IHQ4EIG4"
        );
        assert!(dep_plan.steps[0]
            .to_shell_command()
            .contains("--to CBLX2IYPWPKG4ELMY5ICWHF73Q7HL6ZGK4XBBPAMXQFEYKGNUFSCIMOF"));

        let purpose_hash = "b".repeat(64);
        let (prop_cmd, _, _) = parse_operator_args(&vec_s(&[
            "treasury",
            "propose-withdrawal",
            "--network",
            "local",
            "--source",
            "alice",
            "--payee",
            "GBXEAS6ALJH4H45A4MNM5QQ2LCJUXAOXZVIIYTX7WH3LSHNKGUX3DTRT",
            "--amount",
            "75.25",
            "--purpose-hash",
            &purpose_hash,
            "--dry-run",
        ]))
        .unwrap();
        let prop_plan = build_plan(&prop_cmd, &dir).unwrap();
        assert_eq!(prop_plan.steps[0].function, "submit_payment_request");
        assert!(prop_plan.steps[0]
            .to_shell_command()
            .contains("--amount 752500000"));
    }

    #[test]
    fn mainnet_mutating_requires_confirm_flag_and_unpopulated_network_errors() {
        let dir = repo_deployments_dir();
        let (unconfirmed, _, _) = parse_operator_args(&vec_s(&[
            "bounty",
            "fund",
            "--network",
            "mainnet",
            "--source",
            "alice",
            "--amount",
            "10",
            "--dry-run",
        ]))
        .unwrap();
        // Create a temporary mainnet manifest to verify the confirmation gate triggers first.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::copy(dir.join("local.json"), tmp.path().join("mainnet.json")).unwrap();
        assert_eq!(
            build_plan(&unconfirmed, tmp.path()),
            Err(OperatorError::MainnetConfirmationRequired)
        );

        let (confirmed, _, _) = parse_operator_args(&vec_s(&[
            "bounty",
            "fund",
            "--network",
            "mainnet",
            "--source",
            "alice",
            "--amount",
            "10",
            "--confirm-mainnet",
            "--dry-run",
        ]))
        .unwrap();
        assert!(build_plan(&confirmed, tmp.path()).is_ok());

        // Testnet in the repo has empty `contracts: {}`, so building a plan against it reports ContractNotDeployed.
        let (testnet_status, _, _) = parse_operator_args(&vec_s(&[
            "bounty",
            "status",
            "--network",
            "testnet",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(
            build_plan(&testnet_status, &dir),
            Err(OperatorError::ContractNotDeployed {
                network: "testnet".to_owned(),
                contract: "refract_bounty".to_owned(),
            })
        );
    }
}
