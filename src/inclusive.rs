//! VAT-inclusive pricing: deriving net and tax from a price that already
//! carries the tax.
//!
//! # Why this exists
//!
//! UK retail quotes prices **with VAT in them** — a £12.00 price at 20% is
//! £10.00 net plus £2.00 tax — while the ledger and the filing need the net
//! and the tax separately. The derivation runs the tax arithmetic
//! *backwards*:
//!
//! ```text
//! vat = gross × rate / (1000 + rate)
//! net = gross − vat
//! ```
//!
//! and the two invariants that make it correct: **net + vat equals the gross
//! exactly** (the difference is computed as `gross − vat`, never
//! independently), and the division rounds **once**, through the exact
//! integer path every other tax figure in the estate uses.
//!
//! # The rounding rule, and why it is not free choice
//!
//! HMRC's VAT guide accepts any reasonable rounding provided it is
//! consistent; the estate's choice is the same tie mode every other figure
//! uses, passed explicitly rather than inherited. On a single line the
//! choice moves at most a half-penny; across a thousand-line retail docket
//! an inconsistent choice is a filing discrepancy.

use vat_rules::TieMode;

/// A VAT-inclusive price split into its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrossSplit {
    /// The net amount, minor units — `gross − vat`.
    pub net_minor: i64,
    /// The tax, minor units.
    pub vat_minor: i64,
}

/// Why a VAT-inclusive price cannot be split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrossSplitError {
    /// A rate of 1000‰ (100%) or above: the backwards division is
    /// degenerate (`gross × 1000 / 2000` is a half, not a tax) and no real
    /// consumption tax reaches it. `ap-kit` refuses these too.
    RateAtOrAboveHundredPercent(u32),
}

/// Split a VAT-inclusive gross into net and tax.
///
/// `gross_minor` is the price **as the customer sees it**, including tax;
/// `rate_permille` is the tax rate (200 for 20%). Works for credit-note
/// magnitudes too — a negative gross splits symmetrically, and the pieces
/// still sum to it exactly.
pub fn split_vat_inclusive(
    gross_minor: i64,
    rate_permille: u32,
    mode: TieMode,
) -> Result<GrossSplit, GrossSplitError> {
    if rate_permille >= 1000 {
        return Err(GrossSplitError::RateAtOrAboveHundredPercent(rate_permille));
    }
    // The tax inside a gross amount: gross × r/(1000+r), rounded once.
    // Integer arithmetic on i128 — minor units times a per-mille rate is
    // far inside i64, but the headroom costs nothing and the estate does
    // not sit near edges.
    let numerator = i128::from(gross_minor) * i128::from(rate_permille);
    let denominator = i128::from(1000 + rate_permille);
    let sign = if numerator < 0 { -1i128 } else { 1i128 };
    let magnitude = numerator.abs();
    let quotient = magnitude / denominator;
    let remainder = magnitude % denominator;
    let rounded = if remainder == 0 {
        quotient
    } else {
        let doubled = remainder * 2;
        let takes_upper = doubled > denominator
            || (doubled == denominator && mode.takes_upper_on_tie(quotient % 2 == 0));
        if takes_upper { quotient + 1 } else { quotient }
    };
    let vat = (rounded * sign) as i64;
    Ok(GrossSplit {
        net_minor: gross_minor - vat,
        vat_minor: vat,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_uk_textbook_case_splits_exactly() {
        // £12.00 including VAT at 20%: £10.00 net + £2.00 tax, no rounding
        // involved and not a float on the path.
        let split = split_vat_inclusive(120_000, 200, TieMode::HalfUp).expect("splits");
        assert_eq!(split.net_minor, 100_000);
        assert_eq!(split.vat_minor, 20_000);
        assert_eq!(split.net_minor + split.vat_minor, 120_000);
    }

    #[test]
    fn the_pieces_always_sum_to_the_gross_even_when_rounding() {
        // £0.07 at 20%: the tax is 1.1667p — rounds to 1p, and the net takes
        // the remainder. The pieces sum to the gross by construction (the
        // net is computed as gross − vat), which is the invariant a till
        // receipt needs.
        for gross in [7i64, 1, 99, 123_456, 5] {
            let split = split_vat_inclusive(gross, 200, TieMode::HalfUp).expect("splits");
            assert_eq!(split.net_minor + split.vat_minor, gross, "gross {gross}");
            assert!(split.vat_minor >= 0);
        }
    }

    #[test]
    fn the_tie_moves_by_the_passed_mode_not_by_accident() {
        // 9 minor at 20%: tax = 1800/1200 = exactly 1.5.
        assert_eq!(
            split_vat_inclusive(9, 200, TieMode::HalfUp)
                .expect("splits")
                .vat_minor,
            2
        );
        assert_eq!(
            split_vat_inclusive(9, 200, TieMode::HalfEven)
                .expect("splits")
                .vat_minor,
            2
        );
        assert_eq!(
            split_vat_inclusive(9, 200, TieMode::TowardZero)
                .expect("splits")
                .vat_minor,
            1
        );
        // And the net absorbs the tie's direction either way.
        for mode in [TieMode::HalfUp, TieMode::HalfEven, TieMode::TowardZero] {
            let split = split_vat_inclusive(9, 200, mode).expect("splits");
            assert_eq!(split.net_minor + split.vat_minor, 9);
        }
    }

    #[test]
    fn a_credit_note_magnitude_splits_symmetrically() {
        let split = split_vat_inclusive(-120_000, 200, TieMode::HalfUp).expect("splits");
        assert_eq!(split.net_minor, -100_000);
        assert_eq!(split.vat_minor, -20_000);
        assert_eq!(split.net_minor + split.vat_minor, -120_000);
    }

    #[test]
    fn a_hundred_percent_rate_is_refused_as_degenerate() {
        // gross × 1000/2000 is a half, not a tax: refused, matching ap-kit's
        // implausible-rate gate.
        assert_eq!(
            split_vat_inclusive(100, 1000, TieMode::HalfUp),
            Err(GrossSplitError::RateAtOrAboveHundredPercent(1000))
        );
        assert_eq!(
            split_vat_inclusive(100, 1500, TieMode::HalfUp),
            Err(GrossSplitError::RateAtOrAboveHundredPercent(1500))
        );
    }

    #[test]
    fn the_zero_rate_hides_nothing() {
        // Zero-rated goods: the whole gross is the net, tax is zero — the
        // boring case, kept so a refactor cannot lose it.
        let split = split_vat_inclusive(50_000, 0, TieMode::HalfUp).expect("splits");
        assert_eq!(split.net_minor, 50_000);
        assert_eq!(split.vat_minor, 0);
    }
}
