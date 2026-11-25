#!/usr/bin/env python3
"""Tests for verify_build.sh mismatch reporting (#649)."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "verify_build.sh"


CARGO_STUB = """#!/bin/sh
count_file="$STUB_COUNTER"
count=$(cat "$count_file" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$count_file"
target="$CARGO_TARGET_DIR"
manifest=""
while [ $# -gt 0 ]; do
  case "$1" in
    --target-dir) target=$2; shift 2 ;;
    --manifest-path) manifest=$2; shift 2 ;;
    *) shift ;;
  esac
done
name=$(basename "$(dirname "$manifest")")
mkdir -p "$target/wasm32-unknown-unknown/release"
printf 'build-%s' "$count" > "$target/wasm32-unknown-unknown/release/refract_${name}.wasm"
"""


class VerifyBuildReportingTest(unittest.TestCase):
    def test_reproducibility_mismatch_names_both_artifacts_and_hashes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            shutil.copy(SCRIPT, root / "scripts" / "verify_build.sh")
            (root / "alpha").mkdir()
            (root / "alpha" / "Cargo.toml").write_text("[package]\nname='alpha'\n")
            bin_dir = root / "bin"
            bin_dir.mkdir()
            counter = root / "counter"
            for name, body in (
                ("git", "#!/bin/sh\necho 0123456789abcdef\n"),
                ("rustc", "#!/bin/sh\necho 'rustc 1.0.0'\n"),
                ("cargo", CARGO_STUB),
            ):
                path = bin_dir / name
                path.write_text(body)
                path.chmod(0o755)

            env = os.environ.copy()
            env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
            env["STUB_COUNTER"] = str(counter)
            env["CONTRACTS"] = "alpha"
            env.pop("CONTRACT_IDS", None)
            out_dir = root / "attestations"
            result = subprocess.run(
                ["bash", str(root / "scripts" / "verify_build.sh"), str(out_dir)],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
            )
            stderr = result.stderr
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("NOT REPRODUCIBLE: alpha artifacts differ", stderr)
            self.assertIn("expected ", stderr)
            self.assertIn("actual ", stderr)
            self.assertIn("wasm32-unknown-unknown/release/refract_alpha.wasm", stderr)
            self.assertIn(
                '"reproducible": false', (out_dir / "alpha.json").read_text()
            )


if __name__ == "__main__":
    unittest.main()
