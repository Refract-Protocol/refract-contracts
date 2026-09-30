#!/usr/bin/env python3
"""
Numerical Precision Analysis Script for _calc_premium.
Quantifies precision loss compounding across sequential divisions vs deferred division.
"""

ONE_USDC = 10_000_000 # 1e7
BPS = 10_000
PRECISION = 10_000_000

def test_sweep():
    base_rate_bps = 300 # 3%
    cases = [
        (100 * ONE_USDC, 1, 100, "Min coverage, 1 day, 1.0x"),
        (100 * ONE_USDC, 7, 100, "Min coverage, 7 days, 1.0x"),
        (100 * ONE_USDC, 30, 200, "Min coverage, 30 days, 2.0x"),
        (1_000 * ONE_USDC, 30, 150, "1,000 USDC, 30 days, 1.5x"),
        (10_000 * ONE_USDC, 90, 300, "10,000 USDC, 90 days, 3.0x"),
        (50_000 * ONE_USDC, 365, 300, "50,000 USDC max, 365 days, 3.0x"),
    ]

    print("| Coverage (USDC) | Days | Multiplier | True Premium | Prior (3-Div) | Deferred Div | Error Reduction |")
    print("|---|---|---|---|---|---|---|")

    for cov, days, mult, desc in cases:
        true_val = (cov) * (base_rate_bps / 10000.0) * (days / 365.0) * (mult / 100.0)
        
        # Prior 3-division formulation
        base = cov * base_rate_bps // BPS
        dur = days * PRECISION // 365
        prior = base * dur // PRECISION * mult // 100

        # Optimized deferred division
        num = cov * base_rate_bps * days * mult
        den = BPS * 365 * 100
        deferred = num // den

        err_prior = abs(prior - true_val)
        err_def = abs(deferred - true_val)
        diff_bps = (abs(prior - deferred) / true_val * 10000) if true_val > 0 else 0

        print(f"| {cov/ONE_USDC:,.0f} | {days} | {mult/100:.1f}x | {true_val/ONE_USDC:.6f} | {prior/ONE_USDC:.6f} | {deferred/ONE_USDC:.6f} | {err_prior - err_def:.1f} stroops ({diff_bps:.2f} bps) |")

if __name__ == "__main__":
    test_sweep()
