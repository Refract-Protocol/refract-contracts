#!/usr/bin/env python3
"""Unit tests for the `deny.toml` supply-chain, license, and source policy (#236)."""

import os
import tomllib
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DENY_TOML = os.path.join(ROOT, "deny.toml")
CARGO_TOML = os.path.join(ROOT, "Cargo.toml")
AUDIT_TOML = os.path.join(ROOT, "audit.toml")

GOVERNANCE_ADJACENT_CRATES = ("backstop", "bounty", "grants", "treasury")


def evaluate_dependency_against_policy(policy, *, license_id, registry_url, git_url=None, version_req="0.1.0", yanked=False):
    """Pure policy evaluator used to self-test `deny.toml` rules against synthetic bad crates."""
    if yanked and policy["advisories"].get("yanked") == "deny":
        return False, "yanked crate denied by [advisories].yanked"
    if version_req == "*" and policy["bans"].get("wildcards") == "deny":
        return False, "wildcard version denied by [bans].wildcards"
    if git_url is not None:
        allowed_git = set(policy["sources"].get("allow-git", []))
        if policy["sources"].get("unknown-git") == "deny" and git_url not in allowed_git:
            return False, f"unapproved git source denied: {git_url}"
    allowed_registries = set(policy["sources"].get("allow-registry", []))
    if policy["sources"].get("unknown-registry") == "deny" and registry_url not in allowed_registries:
        return False, f"unapproved registry denied: {registry_url}"
    allowed_licenses = set(policy["licenses"].get("allow", []))
    if license_id not in allowed_licenses:
        return False, f"unapproved license denied: {license_id}"
    return True, "ok"


class DenyPolicyTest(unittest.TestCase):
    def setUp(self):
        with open(DENY_TOML, "rb") as f:
            self.policy = tomllib.load(f)
        with open(CARGO_TOML, "rb") as f:
            self.workspace = tomllib.load(f)
        with open(AUDIT_TOML, "rb") as f:
            self.audit = tomllib.load(f)

    def test_four_governance_adjacent_crates_are_in_workspace_members(self):
        members = set(self.workspace["workspace"]["members"])
        excludes = set(self.workspace["workspace"].get("exclude", []))
        for crate in GOVERNANCE_ADJACENT_CRATES:
            self.assertIn(crate, members, f"{crate} must be in [workspace].members")
            self.assertNotIn(crate, excludes, f"{crate} must not be excluded from workspace")

    def test_advisory_policy_denies_yanked_and_aligns_with_audit_toml(self):
        adv = self.policy["advisories"]
        self.assertEqual(adv["version"], 2)
        self.assertEqual(adv["yanked"], "deny")
        self.assertIn("https://github.com/rustsec/advisory-db", adv["db-urls"])
        accepted_ids = [e["id"] for e in self.audit.get("accepted", [])]
        self.assertEqual(adv.get("ignore", []), accepted_ids)

    def test_license_allowlist_requires_mit_compatible_licenses(self):
        lic = self.policy["licenses"]
        self.assertEqual(lic["version"], 2)
        allowed = set(lic["allow"])
        self.assertIn("MIT", allowed)
        self.assertIn("Apache-2.0", allowed)
        for copyleft in ("GPL-2.0-only", "GPL-3.0-only", "AGPL-3.0-only", "SSPL-1.0"):
            self.assertNotIn(copyleft, allowed)

    def test_bans_and_sources_enforce_crates_io_only_and_no_wildcards(self):
        bans = self.policy["bans"]
        self.assertEqual(bans["wildcards"], "deny")
        sources = self.policy["sources"]
        self.assertEqual(sources["unknown-registry"], "deny")
        self.assertEqual(sources["unknown-git"], "deny")
        self.assertEqual(sources["allow-registry"], ["https://github.com/rust-lang/crates.io-index"])
        self.assertEqual(sources["allow-git"], [])

    def test_known_bad_dependencies_fail_policy_and_clean_dependency_passes(self):
        crates_io = "https://github.com/rust-lang/crates.io-index"
        ok, reason = evaluate_dependency_against_policy(
            self.policy, license_id="MIT", registry_url=crates_io
        )
        self.assertTrue(ok, reason)

        # 1. Copyleft / unapproved license fails
        ok, reason = evaluate_dependency_against_policy(
            self.policy, license_id="AGPL-3.0-only", registry_url=crates_io
        )
        self.assertFalse(ok)
        self.assertIn("unapproved license", reason)

        # 2. Unknown git source fails
        ok, reason = evaluate_dependency_against_policy(
            self.policy,
            license_id="MIT",
            registry_url=crates_io,
            git_url="https://github.com/ untrusted/fork.git",
        )
        self.assertFalse(ok)
        self.assertIn("unapproved git source", reason)

        # 3. Wildcard dependency fails
        ok, reason = evaluate_dependency_against_policy(
            self.policy, license_id="MIT", registry_url=crates_io, version_req="*"
        )
        self.assertFalse(ok)
        self.assertIn("wildcard", reason)

        # 4. Yanked dependency fails
        ok, reason = evaluate_dependency_against_policy(
            self.policy, license_id="MIT", registry_url=crates_io, yanked=True
        )
        self.assertFalse(ok)
        self.assertIn("yanked", reason)

    def test_audit_script_validate_deny_policy_passes_on_clean_repo(self):
        import sys
        sys.path.insert(0, os.path.join(ROOT, "scripts"))
        import audit
        self.assertEqual(audit.validate_deny_policy(), [])


if __name__ == "__main__":
    unittest.main()
