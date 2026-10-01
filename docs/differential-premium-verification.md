# Differential Premium Testing & Precision Verification (Issues #20, #21, #25)

## Overview
This suite implements mathematical precision bounds and differential testing between `RefractPool::_calc_premium` and an independent mathematical reference model.

### Verified Properties
1. **Zero Truncation Compounding**:
   Deferred single-division grouping eliminates intermediate rounding loss, ensuring:
   $$\text{Premium}_{\text{deferred}} \ge \text{Premium}_{\text{naive}}$$
2. **Deterministic Multiplier Pricing**:
   Each coverage type evaluates exactly to its designated risk tier (1.0x for StablecoinDepeg, 1.5x for MarketCrash, etc.).
3. **Per-Holder Index Scaling**:
   Large batches (50+ policies) correctly register and index without vector degradation.
