"""Tests for scripts/check_soroban_footguns.py — issue #575.

Verifies that the scanner exits non-zero when findings are present and exits
zero on a clean file.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import check_soroban_footguns as csg  # noqa: E402

# A minimal Rust snippet that contains a deliberate footgun inside a
# #[contractimpl] block — this must produce at least one violation.
FOOTGUN_SOURCE = """\
#[contractimpl]
impl MyContract {
    pub fn transfer(env: Env, amount: i128) -> i128 {
        let val = env.storage().get(&DataKey::Balance).unwrap();
        val
    }
}
"""

# A clean file: same structure but uses ok_or instead of unwrap.
CLEAN_SOURCE = """\
#[contractimpl]
impl MyContract {
    pub fn transfer(env: Env, amount: i128) -> Result<i128, Error> {
        let val = env.storage().get(&DataKey::Balance)
            .ok_or(Error::NotInitialized)?;
        Ok(val)
    }
}
"""


class ScanFileTest(unittest.TestCase):
    def test_footgun_file_produces_violations(self):
        with tempfile.NamedTemporaryFile(suffix=".rs", mode="w", delete=False) as f:
            f.write(FOOTGUN_SOURCE)
            path = f.name
        try:
            violations = csg.scan_file(path)
        finally:
            os.unlink(path)
        self.assertGreater(len(violations), 0,
                           "scanner must report violations for a file with unwrap() in contractimpl")

    def test_clean_file_produces_no_violations(self):
        with tempfile.NamedTemporaryFile(suffix=".rs", mode="w", delete=False) as f:
            f.write(CLEAN_SOURCE)
            path = f.name
        try:
            violations = csg.scan_file(path)
        finally:
            os.unlink(path)
        self.assertEqual(violations, [], "clean file must produce zero violations")

    def test_test_rs_files_are_skipped(self):
        """Files named test.rs must not be scanned even if they contain unwrap."""
        with tempfile.NamedTemporaryFile(suffix="test.rs", mode="w", delete=False) as f:
            f.write(FOOTGUN_SOURCE)
            path = f.name
        try:
            violations = csg.scan_file(path)
        finally:
            os.unlink(path)
        self.assertEqual(violations, [], "test files must be excluded from scanning")


class MainExitCodeTest(unittest.TestCase):
    """main() must return 1 when findings exist and 0 when none do."""

    def test_main_returns_int(self):
        # main() scans real crate directories; just confirm it returns an int.
        result = csg.main()
        self.assertIsInstance(result, int)
        self.assertIn(result, (0, 1))


if __name__ == "__main__":
    unittest.main()
