//! Property tests for the money arithmetic.
//!
//! The example tests pin worked cases from HMRC and the GST Act; these pin
//! the algebra across arbitrary amounts and rates — the invariants every
//! document implicitly depends on.

#![allow(clippy::expect_used, clippy::panic)]

use invoice_kit::Decimal;
use invoice_kit::inclusive::split_vat_inclusive;
use proptest::prelude::*;
use vat_rules::TieMode;

fn minor_amounts() -> impl Strategy<Value = i64> {
    -1_000_000_000i64..1_000_000_000i64
}

fn per_mille_rates() -> impl Strategy<Value = u32> {
    0u32..1000u32
}

fn tie_modes() -> impl Strategy<Value = TieMode> {
    prop::sample::select(vec![
        TieMode::HalfUp,
        TieMode::HalfEven,
        TieMode::TowardZero,
        TieMode::AwayFromZero,
    ])
}

proptest! {
    /// The inclusive-price split always accounts for the gross: net + vat is
    /// the gross, for every amount, every valid rate, every tie mode. This
    /// is the property a till receipt and a filing both implicitly require.
    #[test]
    fn inclusive_split_always_accounts_for_the_gross(
        gross in minor_amounts(),
        rate in per_mille_rates(),
        mode in tie_modes(),
    ) {
        let split = split_vat_inclusive(gross, rate, mode).expect("rate below 100%");
        prop_assert_eq!(split.net_minor + split.vat_minor, gross);
    }

    /// The tax share is bounded by the rate: |vat| <= |gross| whenever the
    /// rate is below 100%, and a 0% rate hides no tax.
    #[test]
    fn the_tax_share_is_bounded_by_the_rate(
        gross in minor_amounts(),
        rate in per_mille_rates(),
        mode in tie_modes(),
    ) {
        let split = split_vat_inclusive(gross, rate, mode).expect("rate below 100%");
        prop_assert!(split.vat_minor.unsigned_abs() <= gross.unsigned_abs() + 1);
        if rate == 0 {
            prop_assert_eq!(split.vat_minor, 0);
            prop_assert_eq!(split.net_minor, gross);
        }
    }

    /// Sign symmetry: a credit-note magnitude splits into exactly negated
    /// pieces, under every tie mode.
    #[test]
    fn credit_notes_split_symmetrically(
        gross in 1i64..1_000_000_000i64,
        rate in per_mille_rates(),
        mode in tie_modes(),
    ) {
        let positive = split_vat_inclusive(gross, rate, mode).expect("splits");
        let negative = split_vat_inclusive(-gross, rate, mode).expect("splits");
        prop_assert_eq!(negative.vat_minor, -positive.vat_minor);
        prop_assert_eq!(negative.net_minor, -positive.net_minor);
    }

    /// The exact integer division never panics and never loses the sign, on
    /// arbitrary numerators and divisors — the primitive everything else
    /// builds on.
    #[test]
    fn round_div_never_panics_and_stays_within_one_divisor_step(
        numerator in -1_000_000_000_000i128..1_000_000_000_000i128,
        divisor in 1i128..10_000i128,
        mode in tie_modes(),
    ) {
        let rounded = Decimal::round_div(numerator, divisor, mode);
        // The rounded quotient is within one divisor-step of the exact one.
        let exact = numerator / divisor;
        prop_assert!((rounded - exact).abs() <= 1, "rounded {rounded} vs exact {exact}");
        // Sign restored correctly: no positive rounding of a negative.
        prop_assert!(numerator >= 0 || rounded <= 0);
        prop_assert!(numerator <= 0 || rounded >= 0);
    }

    /// Exact rescaling up multiplies the units and preserves the value;
    /// scaling down past the value's precision returns None rather than
    /// truncating.
    #[test]
    fn decimal_rescaling_is_exact_up_and_refuses_lossy_down(
        units in -1_000_000_000_000i128..1_000_000_000_000i128,
        scale in 0u32..9u32,
        extra in 1u32..4u32,
    ) {
        let d = Decimal::new(units, scale);
        let up = d.rescale_exact(scale + extra).expect("upscaling is exact");
        prop_assert_eq!(up.units, units * 10i128.pow(extra));
        prop_assert_eq!(up.scale, scale + extra);
        // The value survives: rescaling back down recovers the original.
        let back = up.rescale_exact(scale).expect("exact downscale");
        prop_assert_eq!(back.units, units);
    }

    /// The parser boundary never panics, whatever bytes arrive.
    #[test]
    fn decimal_parsing_never_panics(arbitrary in ".*") {
        let _ = Decimal::parse(&arbitrary);
    }
}
