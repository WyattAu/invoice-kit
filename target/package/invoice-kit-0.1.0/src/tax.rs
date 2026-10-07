//! Tax categories and the codes a document must carry on the wire.
//!
//! EN 16931 puts **two independent vocabularies** on every taxable line, and
//! conflating them is a rejection cause:
//!
//! - the **category** (BT-151 / BT-118, UNCL5305): `S`, `Z`, `E`, `AE`, `K`,
//!   `G`, `O`, `L`, `M`;
//! - the **exemption reason code** (BT-121, CEF `VATEX-*`): a separate list,
//!   `VATEX-EU-AE`, `VATEX-EU-IC` and so on.
//!
//! A line carries both, and a category without its matching reason code is
//! incomplete even when the rate is right.

/// The UNCL5305 tax category for a taxable item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaxCategory {
    /// `S` — standard rate. **Requires** a non-zero rate (BR-CO-4).
    Standard,
    /// `Z` — zero-rated. Rate must be zero, but the VAT is in the price.
    ZeroRated,
    /// `E` — exempt. Rate and tax amount must be zero (BR-E-*).
    Exempt,
    /// `AE` — reverse charge. Rate and tax must be zero (BR-AE-5..9), both
    /// parties' VAT IDs must be present, and the reason code must be
    /// `VATEX-EU-AE`. All-or-none: a document may not mix AE and non-AE lines
    /// (BR-AE-1).
    ReverseCharge,
    /// `K` — intra-community supply. Zero VAT, reason `VATEX-EU-IC`, plus a
    /// delivery date **and** country of delivery (BR-IC-11/12).
    IntraCommunity,
    /// `G` — free export.
    FreeExport,
    /// `O` — not subject to VAT (outside the scope).
    NotSubject,
    /// `L` — Canary Islands IGIC.
    CanaryIslandsIgic,
    /// `M` — Ceuta/Melilla IPSI.
    CeutaMelillaIpsi,
}

impl TaxCategory {
    /// The UNCL5305 code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Standard => "S",
            Self::ZeroRated => "Z",
            Self::Exempt => "E",
            Self::ReverseCharge => "AE",
            Self::IntraCommunity => "K",
            Self::FreeExport => "G",
            Self::NotSubject => "O",
            Self::CanaryIslandsIgic => "L",
            Self::CeutaMelillaIpsi => "M",
        }
    }

    /// Whether this category is expected to carry a zero tax amount.
    pub fn expects_zero_tax(self) -> bool {
        !matches!(self, Self::Standard)
    }

    /// The CEF `VATEX-*` exemption reason code this category requires.
    ///
    /// `None` for the categories that need no reason, which is only `S`.
    pub fn exemption_reason(self) -> Option<&'static str> {
        match self {
            Self::ReverseCharge => Some("VATEX-EU-AE"),
            Self::IntraCommunity => Some("VATEX-EU-IC"),
            Self::Exempt => Some("VATEX-EU-D"),
            Self::FreeExport => Some("VATEX-EU-G"),
            Self::CanaryIslandsIgic => Some("VATEX-EU-L"),
            Self::CeutaMelillaIpsi => Some("VATEX-EU-M"),
            Self::NotSubject => Some("VATEX-EU-O"),
            Self::ZeroRated => Some("VATEX-EU-Z"),
            Self::Standard => None,
        }
    }

    /// Whether this category's rate must be zero.
    ///
    /// Zero-rated is the trap: its VAT is *in* the price, so the computed tax is
    /// zero, but it is not an exemption and it still needs a reason code.
    pub fn rate_must_be_zero(self) -> bool {
        !matches!(self, Self::Standard)
    }

    /// Whether this category requires a buyer VAT identifier.
    ///
    /// EN 16931 makes BT-49 conditional rather than mandatory, which is why a
    /// validator accepts an invoice a tax authority would reject.
    pub fn requires_buyer_vat_id(self) -> bool {
        matches!(
            self,
            Self::ReverseCharge
                | Self::IntraCommunity
                | Self::CanaryIslandsIgic
                | Self::CeutaMelillaIpsi
        )
    }
}

/// Why a tax computation is invalid.
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

/// Whether the document mixes reverse-charged and ordinary lines.
///
/// EN 16931's BR-AE-1 is all-or-none, and a document that mixes them is
/// refused by a validator — but only if the validator checks, and the mixing is
/// easy to produce because the categories are per line.
#[must_use]
pub fn is_all_or_none_reverse_charge(categories: &[TaxCategory]) -> bool {
    let reverse = categories
        .iter()
        .filter(|c| **c == TaxCategory::ReverseCharge)
        .count();
    reverse == 0 || reverse == categories.len()
}
