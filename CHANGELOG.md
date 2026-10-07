# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

## [0.2.1] - 2026-10-06

### Fixed

- **`RoundingPolicy::GstTotalInvoice` did not implement the total-invoice
  rule.** All three Australian modes rounded per (category, rate) group, so
  `GstTotalInvoice` was `En16931Group` with a different name. Under GST Act
  s9-90 the total-invoice method adds *unrounded* GST per taxable supply and
  rounds once; the difference is real money. Worked case, now a test: two lines
  of 5c at 10% and 5c at 30% are 3c under the per-group rule and 2c under the
  total-invoice rule.
- **`RoundingPolicy::GstTaxableSupply` rounded per group, not per supply.**
  s9-90's taxable-supply method rounds each supply to the recorded precision
  before summing.
- **The breakdown no longer disagrees with the total under a single rounding.**
  BR-S-08/S-09 require the BT-151/152 breakdown to sum to the document total,
  but independently-rounded groups cannot do that by construction. The
  residual is now attributed to the largest unrounded group, deterministically.
- **`Decimal::round_div` lost a cent on negative ties**, and could round the
  wrong way under half-even. The tie comparison now happens on the magnitude
  with the sign restored at the end, because truncation toward zero makes
  "which neighbour is even" ill-defined for a negative quotient: -0.5 rounded to
  -1 purely because 0 happens to be even, when the even candidate is 0 and the
  answer is 0.

Six new tests. Three of them fail against the previous implementation.


## [0.2.0] - 2026-10-06

### Changed (breaking)

- **The tax and document vocabulary moved to `vat-rules 0.1.0`** and is
  re-exported here, so `invoice_kit::TaxCategory` and
  `invoice_kit::DocumentType` keep working but a consumer that also depends on
  accounts payable sees *one* `TaxCategory` rather than two structurally
  identical ones that cannot be compared.
- `Decimal::quantize` takes `vat_rules::TieMode` instead of
  `double_entry::RoundingMode`. The tie direction is policy vocabulary, not
  arithmetic machinery, and a crate holding a table of tax codes has no business
  depending on a double-entry engine to express "round ties away from zero".

### Changed

- Totals are now summed as plain integers in minor units rather than through
  `Amount::checked_add`. Summing whole minor units has no tie to resolve, so the
  rounding mode is irrelevant there, and routing it through the ledger's adder
  meant mapping one rounding vocabulary onto another for no reason.

## [0.1.0] - 2026-10-06

Initial release: accounts-receivable invoicing written against EN 16931, Peppol
BIS 3.0, EU Directive 2006/112/EC, HMRC VAT guidance, ZATCA's resolution and
the Australian GST Act.

### Added

- `Invoice` with `TaxDocumentState`, `SettlementState` and the ledger's own
  posting state kept **separate**, because the sources define three orthogonal
  axes and the reference implementations disagree about all three.
- `DocumentType` carrying direction. Every amount is non-negative; a credit note
  has the same shape as an invoice and the opposite meaning.
- `RoundingPolicy` — `En16931Group`, `GstTotalInvoice`, `GstTaxableSupply`,
  `HmrcSeventeenFive` — required, stored on the document, never inferred from a
  platform default.
- `tax`: UNCL5305 categories with their separate CEF `VATEX-*` reason codes, and
  the BR-AE-1 all-or-none reverse-charge rule.
- `allocation`: integer-minor-unit allocation, FIFO (`allocate`) and
  caller-directed (`allocate_specific`), with the residue reported rather than
  escaped.
- `numbering`: gapless series with an optional SHA-256 chain over the previous
  document's hash, and `verify_gapless` for the UK rule that a gap must be
  explained by a retained cancelled document.
- `Decimal`: exact integer-at-scale parsing, with no float anywhere. Chosen
  because three separate production incidents in a widely deployed accounting
  system were caused by float arithmetic on allocation — a one-cent residue, a
  comparison against a rounded display value, and a payment refused outright.

### Bugs this crate's own tests caught before publication

- `checked_div` carried its result at `GUARD + s2 - s1` digits instead of
  `GUARD`, inflating **every quotient by a power of ten**. 60.00 divided by 1
  came out as 6000.00.
- The line arithmetic applied the quantity **twice**, doubling every line.
- `line_net_amounts` used `rescale_exact`, which cannot express a rounding, so
  every unit price that was not an exact multiple of its base quantity produced
  **no line amount at all** — and Peppol §10.2 requires that quotient to be
  rounded. Replaced with a named `Decimal::quantize` carrying the policy's mode.
- The journal entry **debited tax payable**, which is a liability and therefore
  increases on the credit side. The entry was unbalanced by the tax amount and
  would have posted a wrong ledger.
- `post_to` flattened every ledger failure into `NotADraft`, sending the caller
  hunting for a document problem when the real one was a concurrent writer. It
  now returns the ledger's own error.
