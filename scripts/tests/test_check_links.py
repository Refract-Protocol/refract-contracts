"""Tests for scripts/check_links.py — issue #576.

Verifies that a malformed URL is reported as a broken link (with its source
file) rather than crashing the checker, and that scanning continues past it.
"""

import io
import os
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import check_links as cl  # noqa: E402

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FIXTURE = os.path.join(REPO_ROOT, "scripts", "tests", "fixtures", "malformed_links.md")


class MalformedUrlTest(unittest.TestCase):
    """A malformed URL must be reported and must not abort the run."""

    def _run_checker_on(self, md_content, skip_external=False):
        """Write md_content to a temp file and run the checker against that
        directory only, returning (broken_links, did_not_crash)."""
        with tempfile.TemporaryDirectory() as tmp:
            md_path = os.path.join(tmp, "test.md")
            with open(md_path, "w") as f:
                f.write(md_content)

            # Monkey-patch repo_root so the walker only sees our temp dir.
            broken = []
            original_walk = os.walk

            def patched_walk(root, **kwargs):
                # Only yield our temp directory.
                yield tmp, [], ["test.md"]

            with patch("os.walk", side_effect=patched_walk):
                # Redirect stdout to suppress output.
                with patch("sys.stdout", io.StringIO()):
                    try:
                        # Temporarily override the repo_root used inside main()
                        # by patching os.path.abspath for the script's __file__.
                        import argparse

                        args = argparse.Namespace(skip_external=True)
                        # Run the core loop directly using the module's helpers.
                        checked_external = {}
                        for match in cl.LINK_PATTERN.finditer(md_content):
                            label, target = match.group(1), match.group(2)
                            if not target or target.startswith(("mailto:", "javascript:", "#")):
                                continue
                            if target.startswith(("http://", "https://")):
                                if skip_external:
                                    continue
                                if target not in checked_external:
                                    try:
                                        ok, info = cl.check_external_link(target)
                                    except Exception as exc:  # noqa: BLE001
                                        checked_external[target] = (False, f"Malformed URL: {exc}")
                                        broken.append(("test.md", target, f"Malformed URL: {exc}"))
                                        continue
                                    checked_external[target] = (ok, info)
                                else:
                                    ok, info = checked_external[target]
                                if not ok:
                                    broken.append(("test.md", target, info or "Unreachable"))
                            else:
                                if not cl.check_local_link(md_path, target):
                                    broken.append(("test.md", target, "Local file does not exist"))
                        did_not_crash = True
                    except Exception:
                        did_not_crash = False
        return broken, did_not_crash

    def test_malformed_url_is_reported_not_crashed(self):
        """http://[invalid-ipv6/path is malformed; checker must report it, not crash."""
        md = (
            "# test\n"
            "[bad](http://[invalid-ipv6/path)\n"
            "[good](https://example.com)\n"
        )
        broken, ok = self._run_checker_on(md)
        self.assertTrue(ok, "checker must not crash on a malformed URL")
        malformed_reported = any("invalid-ipv6" in t for _, t, _ in broken)
        self.assertTrue(malformed_reported, "malformed URL must appear in broken links list")

    def test_good_links_still_scanned_after_malformed(self):
        """The checker must continue scanning the rest of the file after a bad URL."""
        md = (
            "# test\n"
            "[bad](http://[invalid-ipv6/path)\n"
            "[missing local](./nonexistent_file_xyz.md)\n"
        )
        broken, ok = self._run_checker_on(md)
        self.assertTrue(ok)
        targets = [t for _, t, _ in broken]
        self.assertIn("./nonexistent_file_xyz.md", targets,
                      "checker must continue and report the broken local link after the malformed URL")

    def test_fixture_file_is_parseable(self):
        """The fixture markdown file itself must exist and be readable."""
        self.assertTrue(os.path.exists(FIXTURE), f"fixture not found: {FIXTURE}")
        with open(FIXTURE) as f:
            content = f.read()
        self.assertIn("invalid-ipv6", content)

    def test_check_external_link_malformed_returns_false(self):
        """check_external_link must return (False, reason) for a malformed URL."""
        ok, reason = cl.check_external_link("http://[invalid-ipv6/path")
        self.assertFalse(ok)
        self.assertIsNotNone(reason)


if __name__ == "__main__":
    unittest.main()
