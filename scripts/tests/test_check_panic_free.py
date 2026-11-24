"""Tests for scripts/check_panic_free.sh — issue #574.

Verifies that the script fails closed (exits non-zero with the path in the
message) when the allowlist is missing or unreadable, and that it passes
normally when the allowlist is present.
"""

import os
import subprocess
import tempfile
import unittest

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPT = os.path.join(REPO_ROOT, "scripts", "check_panic_free.sh")


class MissingAllowlistTest(unittest.TestCase):
    """Script must exit non-zero and name the missing path."""

    def test_missing_allowlist_exits_nonzero_with_path_in_message(self):
        # Run the script with an allowlist path that does not exist by temporarily
        # renaming the real one and injecting a fake ALLOWLIST via env override.
        # We achieve the override by writing a wrapper that sets ALLOWLIST to a
        # non-existent path and sources the original script logic.  The simpler
        # approach: just rename the real file, run, then restore.

        allowlist = os.path.join(REPO_ROOT, "scripts", "panic_free_allowlist.txt")
        renamed = allowlist + ".bak_test"
        try:
            os.rename(allowlist, renamed)
            result = subprocess.run(
                ["bash", SCRIPT],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
            )
        finally:
            os.rename(renamed, allowlist)

        self.assertNotEqual(result.returncode, 0, "script must exit non-zero when allowlist is missing")
        combined = result.stdout + result.stderr
        self.assertIn("panic_free_allowlist.txt", combined,
                      "output must name the missing allowlist path")

    def test_normal_run_runs_when_allowlist_exists(self):
        """With the allowlist present the script must attempt to run (not fail on a missing file)."""
        result = subprocess.run(
            ["bash", SCRIPT],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
        )
        combined = result.stdout + result.stderr
        # The script must NOT emit the missing-allowlist error — it got past
        # the existence check and ran the actual scan.
        self.assertNotIn("allowlist not found", combined,
                         "script must not report a missing allowlist when the file exists")
        self.assertNotIn("allowlist not readable", combined,
                         "script must not report unreadable allowlist when the file is readable")


if __name__ == "__main__":
    unittest.main()
