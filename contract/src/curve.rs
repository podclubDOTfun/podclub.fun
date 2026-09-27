//! Constant-product bonding-curve math for FastLaunch (pump.fun-style virtual reserves).
//!
//! Each launch trades against a virtual quote reserve — the "shadow"/virtual liquidity, ~$4k
//! (Economy) / ~$10k (Express) expressed in wNEAR yocto — plus the real wNEAR accumulated from
//! buys. The token side starts at the full on-curve supply. Reserves obey x * y = k with:
//!   x = current_near  = virtual_near + real_near      (quote side)
//!   y = token_reserve = on-curve tokens not yet sold  (base side)
//! Buys move wNEAR in and tokens out; sells reverse. The virtual reserve is only a price anchor,
//! never withdrawable — sells can only ever pay out the real wNEAR collected.
//!
//! Everything here is pure integer math (no state, no promises, no balances) so it is fully
//! unit-tested. The a*b intermediate products overflow u128 at launchpad magnitudes (supply ~1e32
//! * near ~1e24 ≈ 1e56), so we widen to u256 for the multiply-then-divide.
use primitive_types::U256;

/// floor(a * b / c) computed over a 256-bit intermediate so `a * b` never wraps. In every call
/// here the result is a proportion bounded by one of the reserves, so it always fits back in u128.
pub fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    debug_assert!(c != 0, "mul_div by zero");
    let r = U256::from(a) * U256::from(b) / U256::from(c);
    r.as_u128()
}

/// Tokens delivered for a buy of `near_in`, constant-product: Δy = y * Δx / (x + Δx).
/// `token_reserve` = on-curve tokens still unsold, `current_near` = virtual_near + real_near.
/// Result is always < `token_reserve` (the curve can never be fully drained by a finite buy).
pub fn tokens_out(token_reserve: u128, current_near: u128, near_in: u128) -> u128 {
    if near_in == 0 || token_reserve == 0 {
        return 0;
    }
    mul_div(token_reserve, near_in, current_near.saturating_add(near_in))
}

/// wNEAR paid out for selling `token_in` back, symmetric: Δx = x * Δy / (y + Δy). Capped at
/// `real_near`: the virtual reserve anchors price but is not real liquidity, so a seller can
/// never withdraw more wNEAR than buyers have actually put in.
pub fn near_out(current_near: u128, token_reserve: u128, token_in: u128, real_near: u128) -> u128 {
    if token_in == 0 || current_near == 0 {
        return 0;
    }
    let out = mul_div(current_near, token_in, token_reserve.saturating_add(token_in));
    out.min(real_near)
}

#[cfg(test)]
mod tests {
    use super::*;

    const E24: u128 = 1_000_000_000_000_000_000_000_000; // 1.0 in 24-decimal yocto units

    #[test]
    fn mul_div_basic_and_flooring() {
        assert_eq!(mul_div(1_000_000, 1_000_000, 2_000_000), 500_000);
        // 1e6 * 1000 / 1_001_000 = 999.000999… -> floors to 999
        assert_eq!(mul_div(1_000_000, 1_000, 1_001_000), 999);
    }

    #[test]
    fn mul_div_survives_u128_overflowing_product() {
        // 8e32 * 1e24 = 8e56, far past u128::MAX (~3.4e38) — must not wrap.
        let token_reserve = 800_000_000u128 * E24; // 8e32
        let out = mul_div(token_reserve, E24, 10_000 * E24 + E24);
        assert!(out > 0 && out < token_reserve);
    }

    #[test]
    fn tokens_out_half_reserve_when_near_in_equals_current_near() {
        // Δx == x  ->  Δy = y * x / 2x = y/2
        assert_eq!(tokens_out(1_000_000, 1_000_000, 1_000_000), 500_000);
    }

    #[test]
    fn tokens_out_zero_inputs() {
        assert_eq!(tokens_out(0, 1_000_000, 1_000_000), 0);
        assert_eq!(tokens_out(1_000_000, 1_000_000, 0), 0);
    }

    #[test]
    fn tokens_out_never_drains_the_reserve() {
        // Even an enormous buy leaves at least one token on the curve.
        let out = tokens_out(1_000_000, 1_000, u128::MAX / 2);
        assert!(out < 1_000_000);
    }

    #[test]
    fn near_out_is_capped_at_real_near() {
        // Formula would pay 750_000 but only 500_000 real wNEAR exists -> capped.
        let out = near_out(1_500_000, 500_000, 500_000, 500_000);
        assert_eq!(out, 500_000);
    }

    #[test]
    fn near_out_uncapped_when_real_near_is_ample() {
        // 2e6 * 1e6 / (1e6 + 1e6) = 1e6, real_near ample -> full payout.
        let out = near_out(2_000_000, 1_000_000, 1_000_000, 10_000_000);
        assert_eq!(out, 1_000_000);
    }

    #[test]
    fn near_out_zero_inputs() {
        assert_eq!(near_out(1_000_000, 1_000_000, 0, 1_000_000), 0);
        assert_eq!(near_out(0, 1_000_000, 1_000, 1_000_000), 0);
    }

    #[test]
    fn buy_then_full_sell_returns_at_most_real_near() {
        // Start: virtual_near=1e28, on-curve supply=8e32, real_near=0.
        let virtual_near = 10_000 * E24; // 1e28
        let supply = 800_000_000u128 * E24; // 8e32
        let near_in = 5 * E24; // buy with 5 wNEAR (real)
        let bought = tokens_out(supply, virtual_near, near_in);
        assert!(bought > 0 && bought < supply);
        // Now real_near = near_in, current_near = virtual_near + near_in, reserve = supply - bought.
        let current_near = virtual_near + near_in;
        let reserve = supply - bought;
        let refund = near_out(current_near, reserve, bought, near_in);
        // A full round-trip can never pay out more real wNEAR than was put in.
        assert!(refund <= near_in, "sell must not exceed real wNEAR in");
    }

    #[test]
    fn stress_many_buys_then_full_dump_never_prints_wnear() {
        // 20 sequential buys of 250 wNEAR against a 1B-token / 5000-wNEAR-virtual curve, then a
        // single dump of the entire circulating amount. Payout is capped at the real wNEAR taken in
        // — the shadow (virtual) reserve is never withdrawable, and nothing overflows at ~1e60.
        let virtual_near = 5_000 * E24;
        let supply = 1_000_000_000u128 * E24;
        let mut current = virtual_near;
        let mut reserve = supply;
        let mut real = 0u128;
        for _ in 0..20 {
            let near_in = 250 * E24;
            let out = tokens_out(reserve, current, near_in);
            assert!(out > 0 && out < reserve, "each buy delivers a sane slice of the reserve");
            reserve -= out;
            real += near_in;
            current += near_in;
        }
        let circulating = supply - reserve;
        let refund = near_out(current, reserve, circulating, real);
        assert!(refund <= real, "dumping everything can never extract more than the real wNEAR in");
    }
}
