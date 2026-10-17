# Storage-Rent Keeper & TTL Extension Guide (`#262`)

## Overview

Soroban persistent and instance storage entries are archived if their TTL drops to `0`.
Each of the four auxiliary contracts (`RefractBackstop`, `RefractBountyEscrow`, `RefractGrants`, and `RefractTreasury`) exposes a permissionless, bounded `bump_ttl(keys, requested_extend)` entrypoint:

- `MIN_TTL_THRESHOLD = 17_280` ledgers (~1 day)
- `MAX_TTL_EXTEND = 535_680` ledgers (~31 days)
- `MAX_BUMP_BATCH = 16` keys per transaction

## Allowlisted Bump Keys

Only protocol-critical instance storage and existing persistent records (`StakerShares`, `Award`, `Grant`) are bumpable. Missing keys or `NonBumpableJunk` variants return `KeyNotBumpable`, preventing attackers from griefing keepers into extending arbitrary storage entries.

## Running the Keeper

```bash
python3 scripts/ttl_keeper.py --dry-run --threshold 17280 --extend-to 535680
```
