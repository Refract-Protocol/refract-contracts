//! Governance-managed asset allowlist with per-asset risk parameters.
//!
//! An asset must be listed before a contract can accept, store or pay it out.
//! Each listed asset has a per-call cap and a total exposure cap. A call is
//! checked before anything moves, and a refused call changes no exposure. An
//! asset can be disabled without losing its record, so exposure can still be
//! released as funds leave.

use std::collections::BTreeMap;

/// An asset's identifier, for example a token contract handle.
pub type AssetId = u64;

/// Risk limits for one asset, in raw token units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskParams {
    /// Largest amount allowed in one call.
    pub max_per_call: i128,
    /// Largest total amount that may be outstanding in this contract at once.
    pub max_exposure: i128,
}

/// Why an allowlist operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistError {
    /// The asset is not on the list.
    NotListed,
    /// The asset is on the list but has been disabled.
    Disabled,
    /// The asset is already on the list.
    AlreadyListed,
    /// The amount is zero or negative, or the risk parameters are invalid.
    InvalidParams,
    /// The amount is over this asset's per-call cap.
    ExceedsPerCall,
    /// The amount would take outstanding exposure over the asset's cap.
    ExceedsExposure,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    params: RiskParams,
    enabled: bool,
    exposure: i128,
}

/// The assets this contract accepts, with their limits and current exposure.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    assets: BTreeMap<AssetId, Entry>,
}

impl Allowlist {
    /// An empty allowlist. Nothing is accepted until an asset is listed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lists an asset. Its exposure starts at zero.
    pub fn list(&mut self, asset: AssetId, params: RiskParams) -> Result<(), AllowlistError> {
        validate(params)?;
        if self.assets.contains_key(&asset) {
            return Err(AllowlistError::AlreadyListed);
        }
        self.assets.insert(
            asset,
            Entry {
                params,
                enabled: true,
                exposure: 0,
            },
        );
        Ok(())
    }

    /// Changes an asset's limits. Its current exposure is kept.
    pub fn set_params(&mut self, asset: AssetId, params: RiskParams) -> Result<(), AllowlistError> {
        validate(params)?;
        let entry = self.entry_mut(asset)?;
        entry.params = params;
        Ok(())
    }

    /// Stops new calls for an asset. Exposure can still be released.
    pub fn disable(&mut self, asset: AssetId) -> Result<(), AllowlistError> {
        self.entry_mut(asset)?.enabled = false;
        Ok(())
    }

    /// Allows new calls for an asset again.
    pub fn enable(&mut self, asset: AssetId) -> Result<(), AllowlistError> {
        self.entry_mut(asset)?.enabled = true;
        Ok(())
    }

    /// Checks a call of `amount` and, if it passes, adds it to the asset's exposure.
    pub fn check_and_record(&mut self, asset: AssetId, amount: i128) -> Result<(), AllowlistError> {
        if amount <= 0 {
            return Err(AllowlistError::InvalidParams);
        }
        let entry = self.entry_mut(asset)?;
        if !entry.enabled {
            return Err(AllowlistError::Disabled);
        }
        if amount > entry.params.max_per_call {
            return Err(AllowlistError::ExceedsPerCall);
        }
        let exposure = entry
            .exposure
            .checked_add(amount)
            .ok_or(AllowlistError::ExceedsExposure)?;
        if exposure > entry.params.max_exposure {
            return Err(AllowlistError::ExceedsExposure);
        }
        entry.exposure = exposure;
        Ok(())
    }

    /// Releases `amount` of exposure, for example when funds leave. Never goes
    /// below zero.
    pub fn release(&mut self, asset: AssetId, amount: i128) -> Result<(), AllowlistError> {
        let entry = self.entry_mut(asset)?;
        entry.exposure = (entry.exposure - amount.max(0)).max(0);
        Ok(())
    }

    /// Outstanding exposure for an asset, if it is listed.
    pub fn exposure(&self, asset: AssetId) -> Option<i128> {
        self.assets.get(&asset).map(|entry| entry.exposure)
    }

    fn entry_mut(&mut self, asset: AssetId) -> Result<&mut Entry, AllowlistError> {
        self.assets.get_mut(&asset).ok_or(AllowlistError::NotListed)
    }
}

fn validate(params: RiskParams) -> Result<(), AllowlistError> {
    if params.max_per_call <= 0 || params.max_exposure < params.max_per_call {
        return Err(AllowlistError::InvalidParams);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: AssetId = 1;
    const PARAMS: RiskParams = RiskParams {
        max_per_call: 100,
        max_exposure: 250,
    };

    fn list() -> Allowlist {
        let mut list = Allowlist::new();
        list.list(USDC, PARAMS).unwrap();
        list
    }

    #[test]
    fn unlisted_assets_are_refused() {
        let mut list = Allowlist::new();
        assert_eq!(
            list.check_and_record(USDC, 1),
            Err(AllowlistError::NotListed)
        );
    }

    #[test]
    fn per_call_and_exposure_caps_are_enforced_without_side_effects() {
        let mut list = list();
        assert_eq!(
            list.check_and_record(USDC, 101),
            Err(AllowlistError::ExceedsPerCall)
        );
        list.check_and_record(USDC, 100).unwrap();
        list.check_and_record(USDC, 100).unwrap();
        // 200 + 100 would exceed the 250 exposure cap.
        assert_eq!(
            list.check_and_record(USDC, 100),
            Err(AllowlistError::ExceedsExposure)
        );
        assert_eq!(list.exposure(USDC), Some(200));
    }

    #[test]
    fn disabling_stops_new_calls_but_allows_release() {
        let mut list = list();
        list.check_and_record(USDC, 100).unwrap();
        list.disable(USDC).unwrap();
        assert_eq!(list.check_and_record(USDC, 1), Err(AllowlistError::Disabled));

        list.release(USDC, 100).unwrap();
        assert_eq!(list.exposure(USDC), Some(0));
        list.enable(USDC).unwrap();
        list.check_and_record(USDC, 1).unwrap();
    }

    #[test]
    fn release_never_goes_below_zero() {
        let mut list = list();
        list.release(USDC, 500).unwrap();
        assert_eq!(list.exposure(USDC), Some(0));
    }

    #[test]
    fn invalid_params_and_duplicates_are_refused() {
        let mut list = list();
        assert_eq!(list.list(USDC, PARAMS), Err(AllowlistError::AlreadyListed));
        assert_eq!(
            list.list(
                2,
                RiskParams {
                    max_per_call: 0,
                    max_exposure: 10,
                }
            ),
            Err(AllowlistError::InvalidParams)
        );
        assert_eq!(
            list.set_params(
                USDC,
                RiskParams {
                    max_per_call: 100,
                    max_exposure: 50,
                }
            ),
            Err(AllowlistError::InvalidParams)
        );
        assert_eq!(list.check_and_record(USDC, 0), Err(AllowlistError::InvalidParams));
    }

    #[test]
    fn params_can_change_and_keep_the_current_exposure() {
        let mut list = list();
        list.check_and_record(USDC, 100).unwrap();
        list.set_params(
            USDC,
            RiskParams {
                max_per_call: 50,
                max_exposure: 1_000,
            },
        )
        .unwrap();
        assert_eq!(list.exposure(USDC), Some(100));
        assert_eq!(
            list.check_and_record(USDC, 60),
            Err(AllowlistError::ExceedsPerCall)
        );
    }
}
