//! Licensing declarations and IP assignment attestation for grant deliverables.
//!
//! A grantee declares, for each milestone, the license their deliverable is
//! released under (an SPDX identifier), and a hash that names the deliverable's
//! content. Once the grantee attests to the IP assignment, the declaration is
//! locked, so the license and deliverable cannot be changed after the fact.

use std::collections::BTreeMap;

/// A grant's identifier.
pub type GrantId = u64;

/// A milestone's index within its grant.
pub type MilestoneIndex = u32;

/// An account's handle.
pub type Account = u64;

/// Content hash of a deliverable.
pub type DeliverableHash = [u8; 32];

/// Why a licensing operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicensingError {
    /// The license identifier is not a valid SPDX-style identifier.
    InvalidLicense(String),
    /// Only the grantee can declare or attest.
    NotRecipient,
    /// The declaration is already attested and cannot change.
    AlreadyAttested,
    /// No declaration exists for this milestone.
    NotDeclared,
}

/// One milestone's licensing declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub license: String,
    pub deliverable: DeliverableHash,
    pub attested: bool,
}

/// Licensing declarations for grant milestones.
#[derive(Debug, Clone, Default)]
pub struct Licensing {
    declarations: BTreeMap<(GrantId, MilestoneIndex), Declaration>,
}

impl Licensing {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares or replaces the license and deliverable for a milestone, before
    /// attestation. Only the grant's recipient can do this.
    pub fn declare(
        &mut self,
        caller: Account,
        recipient: Account,
        grant: GrantId,
        milestone: MilestoneIndex,
        license: &str,
        deliverable: DeliverableHash,
    ) -> Result<(), LicensingError> {
        require_recipient(caller, recipient)?;
        if !is_valid_spdx(license) {
            return Err(LicensingError::InvalidLicense(license.to_string()));
        }
        let key = (grant, milestone);
        if self.declarations.get(&key).is_some_and(|d| d.attested) {
            return Err(LicensingError::AlreadyAttested);
        }
        self.declarations.insert(
            key,
            Declaration {
                license: license.to_string(),
                deliverable,
                attested: false,
            },
        );
        Ok(())
    }

    /// Attests to the IP assignment for a declared milestone. After this the
    /// declaration is locked. Only the grant's recipient can attest.
    pub fn attest(
        &mut self,
        caller: Account,
        recipient: Account,
        grant: GrantId,
        milestone: MilestoneIndex,
    ) -> Result<(), LicensingError> {
        require_recipient(caller, recipient)?;
        let declaration = self
            .declarations
            .get_mut(&(grant, milestone))
            .ok_or(LicensingError::NotDeclared)?;
        if declaration.attested {
            return Err(LicensingError::AlreadyAttested);
        }
        declaration.attested = true;
        Ok(())
    }

    /// The declaration for a milestone, if one exists.
    pub fn declaration(&self, grant: GrantId, milestone: MilestoneIndex) -> Option<&Declaration> {
        self.declarations.get(&(grant, milestone))
    }
}

fn require_recipient(caller: Account, recipient: Account) -> Result<(), LicensingError> {
    if caller != recipient {
        return Err(LicensingError::NotRecipient);
    }
    Ok(())
}

/// Accepts an SPDX-style identifier: ASCII letters, digits, `-`, `.` and `+`,
/// with no leading or trailing `-`. A full SPDX expression with spaces, such as
/// `MIT OR Apache-2.0`, is not accepted here.
fn is_valid_spdx(license: &str) -> bool {
    !license.is_empty()
        && !license.starts_with('-')
        && !license.ends_with('-')
        && license
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPIENT: Account = 2;
    const OTHER: Account = 3;
    const HASH: DeliverableHash = [9u8; 32];

    #[test]
    fn a_valid_declaration_can_be_made_and_attested() {
        let mut licensing = Licensing::new();
        licensing
            .declare(RECIPIENT, RECIPIENT, 1, 0, "Apache-2.0", HASH)
            .unwrap();
        licensing.attest(RECIPIENT, RECIPIENT, 1, 0).unwrap();
        assert!(licensing.declaration(1, 0).unwrap().attested);
    }

    #[test]
    fn invalid_license_identifiers_are_refused() {
        let mut licensing = Licensing::new();
        for bad in ["", "MIT OR Apache-2.0", "-MIT", "MIT/Apache"] {
            assert!(matches!(
                licensing.declare(RECIPIENT, RECIPIENT, 1, 0, bad, HASH),
                Err(LicensingError::InvalidLicense(_))
            ));
        }
    }

    #[test]
    fn an_attested_declaration_cannot_change() {
        let mut licensing = Licensing::new();
        licensing
            .declare(RECIPIENT, RECIPIENT, 1, 0, "MIT", HASH)
            .unwrap();
        licensing.attest(RECIPIENT, RECIPIENT, 1, 0).unwrap();
        assert_eq!(
            licensing.declare(RECIPIENT, RECIPIENT, 1, 0, "GPL-3.0", HASH),
            Err(LicensingError::AlreadyAttested)
        );
        assert_eq!(licensing.attest(RECIPIENT, RECIPIENT, 1, 0), Err(LicensingError::AlreadyAttested));
        assert_eq!(licensing.declaration(1, 0).unwrap().license, "MIT");
    }

    #[test]
    fn only_the_recipient_declares_and_attests() {
        let mut licensing = Licensing::new();
        assert_eq!(
            licensing.declare(OTHER, RECIPIENT, 1, 0, "MIT", HASH),
            Err(LicensingError::NotRecipient)
        );
        licensing
            .declare(RECIPIENT, RECIPIENT, 1, 0, "MIT", HASH)
            .unwrap();
        assert_eq!(
            licensing.attest(OTHER, RECIPIENT, 1, 0),
            Err(LicensingError::NotRecipient)
        );
        assert_eq!(
            licensing.attest(RECIPIENT, RECIPIENT, 1, 9),
            Err(LicensingError::NotDeclared)
        );
    }
}
