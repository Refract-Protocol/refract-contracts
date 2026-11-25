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

### Running the precision analysis

`scripts/numerical_precision_analysis.py` takes no arguments or flags:

```text
python3 scripts/numerical_precision_analysis.py
```

The script prints a Markdown table to stdout, one row per built-in
coverage/days/multiplier case, with the true premium, the prior three-division
result, the deferred-division result, and the error reduction in stroops and
basis points. It writes no files, and there is no flag for selecting cases or
changing output; edit the `cases` list in the script for other inputs.
