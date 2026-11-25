"""Install a minimal ``tomllib`` stand-in on Python < 3.11 for these tests.

The production scripts run on Python 3.11+, where ``tomllib`` is in the
standard library. The test environment here is 3.10, so import this module
before importing a script that uses ``tomllib`` and patch the loader itself
in each test.
"""

import sys
import types

try:
    import tomllib  # noqa: F401
except ModuleNotFoundError:  # Python 3.10
    tomllib = types.ModuleType("tomllib")

    class TOMLDecodeError(ValueError):
        pass

    def load(_fp):
        return {}

    tomllib.TOMLDecodeError = TOMLDecodeError
    tomllib.load = load
    sys.modules["tomllib"] = tomllib
