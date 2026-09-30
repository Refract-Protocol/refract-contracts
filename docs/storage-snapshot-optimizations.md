# Storage Re-Read Elimination via Cached State Snapshot

## Overview
Soroban smart contracts incur host CPU and metering costs on every ledger access via \env.storage()\. Prior to this optimization, core entrypoints (\uy_policy\, \withdraw_capital\, \provide_capital\) redundantly fetched identical ledger keys across multiple private helper invocations.

This optimization introduces a dedicated \PoolState\ snapshot that loads instance configuration and cumulative balances in a single coherent operation, passing immutable references into shared math and capacity helpers.

## Storage Read Reduction & Host Metering Impact

| Entrypoint | Previous Storage Reads | Optimized Storage Reads | Redundant Reads Eliminated | Host Metering Impact |
|---|---|---|---|---|
| \uy_policy\ | 4 reads (\PoolConfig\, \TotalCapital\, \TotalCoverage\, \TotalCapital\ reload) | 1 snapshot read | **-3 redundant reads (-75%)** | Lower CPU instructions & gas |
| \withdraw_capital\ | 6 reads (\PoolConfig\, \TotalCapital\, \TotalCoverage\, \TotalShares\, \PoolConfig\ reload, \TotalCapital\/\Shares\ reload) | 1 snapshot read | **-5 redundant reads (-83%)** | Lower CPU instructions & gas |
| \provide_capital\ | 3 reads (\TotalCapital\, \TotalShares\, and reload) | 1 snapshot read | **-2 redundant reads (-66%)** | Lower CPU instructions & gas |
| \quote_shares\ | 2 reads | 1 snapshot read | **-1 redundant read (-50%)** | Lower read footprint |
| \quote_withdrawal\ | 4 reads | 1 snapshot read | **-3 redundant reads (-75%)** | Lower read footprint |

## Invariant Preservation
- **Quote / Execution Parity**: Both \quote_withdrawal\ and \withdraw_capital\ share \RefractPool::_quote_withdrawal(&state, shares)\.
- **Capacity Floor & Ceiling**: \_check_coverage_capacity(&state, params.coverage_amount)\ enforces identical bounds across quote and buy paths.
- **Authorization Ordering**: \provider.require_auth()\ and \holder.require_auth()\ precede storage access.
