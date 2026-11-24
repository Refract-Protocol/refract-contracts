"""
Tests for the coverage-floor enforcement logic in scripts/coverage.py.

These tests monkey-patch the measurement function so they don't need
cargo llvm-cov installed.  They verify:
  - exit 0 when measured >= floor
  - exit 1 when measured < floor
  - the printed output shows both values and the gap
"""

from __future__ import annotations

import importlib
import sys
import types
from pathlib import Path
from unittest.mock import patch, MagicMock

import pytest

# Allow importing scripts/coverage.py as a module
SCRIPTS_DIR = Path(__file__).parent.parent / "scripts"
sys.path.insert(0, str(SCRIPTS_DIR))


def _reload_coverage_module():
    """Import (or re-import) scripts/coverage.py fresh."""
    if "coverage" in sys.modules:
        del sys.modules["coverage"]
    spec = importlib.util.spec_from_file_location(
        "coverage", SCRIPTS_DIR / "coverage.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def test_passes_when_coverage_meets_floor(tmp_path, capsys):
    """main() exits 0 and prints both values when measured >= floor."""
    cov = _reload_coverage_module()

    with patch.object(cov, "load_floor", return_value=80.0), \
         patch.object(cov, "measure_coverage", return_value=85.5):
        with pytest.raises(SystemExit) as exc_info:
            cov.main()

    # Should NOT raise SystemExit when coverage passes — but main() doesn't
    # call sys.exit(0) explicitly, so check for no exit or exit code 0.
    # If it did raise, the code must be 0.
    if exc_info.value.code is not None:
        assert exc_info.value.code == 0

    captured = capsys.readouterr()
    assert "85.50%" in captured.out
    assert "80.00%" in captured.out
    assert "PASS" in captured.out


def test_fails_when_coverage_below_floor(tmp_path, capsys):
    """main() exits 1 and shows the gap when measured < floor."""
    cov = _reload_coverage_module()

    with patch.object(cov, "load_floor", return_value=80.0), \
         patch.object(cov, "measure_coverage", return_value=72.3):
        with pytest.raises(SystemExit) as exc_info:
            cov.main()

    assert exc_info.value.code == 1

    captured = capsys.readouterr()
    assert "72.30%" in captured.out
    assert "80.00%" in captured.out
    # Gap = 80.0 - 72.3 = 7.70
    assert "7.70%" in captured.out
    assert "FAIL" in captured.out


def test_output_shows_measured_and_floor(capsys):
    """Both the measured value and the floor appear in stdout."""
    cov = _reload_coverage_module()

    with patch.object(cov, "load_floor", return_value=75.0), \
         patch.object(cov, "measure_coverage", return_value=75.0):
        try:
            cov.main()
        except SystemExit:
            pass

    captured = capsys.readouterr()
    assert "75.00%" in captured.out


def test_exactly_at_floor_passes(capsys):
    """Coverage exactly equal to the floor is not a failure."""
    cov = _reload_coverage_module()

    with patch.object(cov, "load_floor", return_value=80.0), \
         patch.object(cov, "measure_coverage", return_value=80.0):
        try:
            cov.main()
        except SystemExit as e:
            assert e.code == 0 or e.code is None

    captured = capsys.readouterr()
    assert "PASS" in captured.out
