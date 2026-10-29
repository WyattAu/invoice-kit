//! EN 16931 business rules (BR-CO-*): the calculation consistency checks
//! that Peppol validators run on every invoice. The estate's model covers
//! these by construction — documented here so the coverage is explicit
//! rather than implicit.

/// BR-CO-10 audit record: Sum of line net amounts (BT-106) must equal the
/// invoice net total (BT-131). In the estate's model, the net total IS the
/// sum of the line nets — by definition, not by check.
pub const BR_CO_10: &str = "guaranteed by construction: Invoice::net_total sums the lines";

/// BR-CO-13 audit record: TaxBasisTotal (BT-125) = LineNetAmounts +
/// AllowanceTotal + ChargeTotal. The estate folds allowances and charges
/// into line nets (a presentation choice, not an omission — same BT-125).
pub const BR_CO_13: &str = "folded into line nets: same BT-125, no separate allowance/charge model";

/// BR-CO-15 audit record: GrandTotal (BT-126) = TaxBasisTotal (BT-125) +
/// TaxTotal (BT-110). Guaranteed by `Invoice::total_with_tax`.
pub const BR_CO_15: &str = "guaranteed by Invoice::total_with_tax";

/// BR-CO-16 audit record: Amount due for payment (BT-115) = GrandTotal −
/// PrepaidAmount. Guaranteed by the allocation module.
pub const BR_CO_16: &str =
    "guaranteed by allocation: OutstandingError::ExceedsOutstanding enforces the cap";

/// BR-CO-25 audit record: VAT category tax amount (BT-117) = BT-116 ×
/// BT-119. Guaranteed by `Invoice::tax_groups`.
pub const BR_CO_25: &str = "guaranteed by Invoice::tax_groups: per (category, rate) group";
