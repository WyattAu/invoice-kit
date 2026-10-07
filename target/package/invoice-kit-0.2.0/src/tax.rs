//! Re-export of the shared tax vocabulary.
//!
//! The table itself lives in [`vat_rules`], because accounts payable needs
//! exactly the same codes and rules. This module keeps the `invoice_kit::tax::`
//! path working for existing callers.

pub use vat_rules::tax::{TaxCategory, is_all_or_none_reverse_charge};

/// Why a tax computation is invalid.
///
/// Kept for API compatibility: the rules that produce it now live with the
/// categories they constrain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaxError {
    /// A standard-rated line with a zero rate.
    StandardWithZeroRate,
    /// A category that requires zero tax, given a non-zero rate.
    NonZeroRateOnZeroCategory {
        /// The category code.
        category: &'static str,
    },
    /// A reverse-charge document that mixes AE and non-AE lines. BR-AE-1.
    MixedReverseCharge,
    /// A reverse-charge line on a document without both parties' VAT IDs.
    ReverseChargeWithoutVatIds,
    /// An intra-community line without a delivery date and country (BR-IC-11/12).
    IntraCommunityWithoutDelivery,
}
