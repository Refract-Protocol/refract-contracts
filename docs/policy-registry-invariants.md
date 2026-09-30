# Policy Registry Property Invariants & Testing Suite

## Overview
This suite implements proptest-driven property verification over the Refract Policy Registry:
- **Counter Invariant**: Total registered policies is always $\ge$ active policies.
- **Deactivation Monotonicity**: Deactivating $k$ active policies decreases `active_policies` count by exactly $k$.
- **Per-Holder Isolation**: Policies registered by Holder A are strictly isolated from Holder B's active index.
- **Non-Existent ID Safety**: Querying unallocated policy IDs safely returns `None` without panic.
