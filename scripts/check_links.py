#!/usr/bin/env python3
"""Check local cross-references and external links in markdown files across the repository.

Scope:
- Verifies that all local relative links (e.g. ./CODE_OF_CONDUCT.md, ./SECURITY.md) exist.
- Verifies external URLs syntax and reachability when network is available.
"""

import argparse
import os
import re
import sys
import urllib.request
import urllib.error

LINK_PATTERN = re.compile(r'\[([^\]]+)\]\(([^)\s]+)(?:\s+"[^"]*")?\)')


def check_local_link(source_file, target):
    target_clean = target.split('#')[0].split('?')[0]
    if not target_clean:
        return True

    source_dir = os.path.dirname(os.path.abspath(source_file))
    target_path = os.path.normpath(os.path.join(source_dir, target_clean))
    return os.path.exists(target_path)


def check_external_link(url, timeout=5):
    req = urllib.request.Request(
        url,
        headers={"User-Agent": "Mozilla/5.0 (compatible; RefractLinkChecker/1.0)"}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return 200 <= response.status < 400, None
    except urllib.error.HTTPError as e:
        if e.code in (403, 405, 429):
            return True, None
        return False, f"HTTP {e.code}"
    except urllib.error.URLError as e:
        # If offline or getaddrinfo fails, don't fail on network-isolated environments
        if "getaddrinfo failed" in str(e.reason) or "Name or service not known" in str(e.reason):
            return True, f"Skipped (DNS/Network unavailable: {e.reason})"
        return False, str(e.reason)
    except Exception as e:
        return False, str(e)


def main():
    parser = argparse.ArgumentParser(description="Check markdown links.")
    parser.add_argument("--skip-external", action="store_true", help="Skip checking external links")
    args = parser.parse_args()

    repo_root = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
    broken_links = []
    checked_external = {}

    print(f"Checking markdown links across {repo_root}...")

    for root, dirs, files in os.walk(repo_root):
        dirs[:] = [d for d in dirs if d not in {".git", "target", "dist", "node_modules", "test_snapshots"}]
        for f in files:
            if not f.endswith(".md"):
                continue
            file_path = os.path.join(root, f)
            rel_path = os.path.relpath(file_path, repo_root)

            with open(file_path, "r", encoding="utf-8") as fp:
                content = fp.read()

            for match in LINK_PATTERN.finditer(content):
                label, target = match.group(1), match.group(2)
                if not target or target.startswith(("mailto:", "javascript:", "#")):
                    continue

                if target.startswith(("http://", "https://")):
                    if args.skip_external:
                        continue
                    if target not in checked_external:
                        ok, info = check_external_link(target)
                        checked_external[target] = (ok, info)
                        if info:
                            print(f"  [URL] {target}: {info}")
                    else:
                        ok, info = checked_external[target]

                    if not ok:
                        broken_links.append((rel_path, target, info or "Unreachable"))
                else:
                    if not check_local_link(file_path, target):
                        broken_links.append((rel_path, target, "Local file does not exist"))

    if broken_links:
        print(f"\nFound {len(broken_links)} broken link(s):")
        for src, target, reason in broken_links:
            print(f"  {src} -> {target} ({reason})")
        sys.exit(1)
    else:
        print("All markdown cross-references and links verified successfully!")
        sys.exit(0)


if __name__ == "__main__":
    main()
