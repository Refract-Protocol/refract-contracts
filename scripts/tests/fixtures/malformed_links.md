# Malformed URL fixture

This file is used by `test_check_links.py` to verify that `check_links.py`
handles malformed URLs gracefully instead of crashing.

A well-formed link (local, exists):
[README](../../README.md)

A malformed URL — invalid IPv6 bracket syntax:
[Bad link](http://[invalid-ipv6/path)

Another well-formed external URL placeholder (will be skipped in tests):
[Example](https://example.com)
