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

## Running the analysis

The precision analysis script lives at `scripts/numerical_precision_analysis.py`.
Run it from the repository root with no additional flags:

```bash
python3 scripts/numerical_precision_analysis.py
```

The script prints a Markdown table comparing the true floating-point premium,
the prior three-division integer formulation, and the deferred-division
optimisation for a set of representative coverage amounts, durations and
multipliers. It exits `0` on success and writes no files.
