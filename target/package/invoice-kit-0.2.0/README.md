# invoice-kit

Accounts-receivable invoicing for an SME ledger, written against primary
sources rather than against another implementation.

Three orthogonal states, no minus signs, and a rounding policy you have to
name. See the crate docs for the reasoning; the short version:

- **EN 16931 defines no lifecycle at all.** It is a semantic data model with no
  `status` element. So the tax-document state, the settlement state and the
  posting state are three separate things here, not one enum. Odoo needs two
  fields for settlement, ERPNext needs three, and the two disagree about
  whether "paid" needs a bank line.
- **Peppol BIS 3.0 §5.6 defines two mutually exclusive credit conventions**, and
  a document using both passes every validator while booking the wrong sign. This
  crate takes the CreditNote convention: the document type carries direction and
  every amount is non-negative. ISO 20022 reaches the same place from the other
  direction, with a separate `CdtDbtInd` indicator rather than a sign.
- **Four jurisdictions mandate mutually incompatible arithmetic.** EN 16931
  rounds at the leaf and sums; Australia's GST Act s9-90 *legally permits*
  rounding the total once instead — a five-cent difference on twenty lines at
  $3.49, lawful there and invalid under EN 16931 — and HMRC's rules "have no
  statutory basis". EN 16931 is silent on tie-breaking, and PostgreSQL's
  `numeric` (ties away from zero) and `double precision` (ties to even) disagree
  with each other. So `RoundingPolicy` is a required argument, and it is stored
  on the document.

## What it enforces

- A corrective document **must** reference another and carry a reason. EU
  Directive 2006/112/EC Article 219 requires a document to amend *and* refer
  specifically and unambiguously; both are conjunctive, and **BR-55 is `0..1` in
  EN 16931 core**, so a credit note with no billing reference passes every
  validator. ZATCA's BR-KSA-17 requires one of five reasons that EN 16931 has no
  field for.
- Line net amount is `round(price / base_quantity, scale) * quantity`, with the
  quotient rounded first — Peppol §10.2, and a different number from the other
  order whenever the base quantity is not 1.
- Tax is computed per `(category, rate)` group, never per line.
- Numbering is **gapless**, because two jurisdictions require it and a third
  tolerates a gap only with the cancelled document retained. A cancelled number
  is spent, never reissued. An optional SHA-256 chain detects deletion from the
  middle of a series, which a gap check cannot see.
- Payment allocation is integer minor units throughout, with the rounding
  residue reported rather than parked in an unaccounted balance.

## Licence

MIT OR Apache-2.0.
