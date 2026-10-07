# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

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
