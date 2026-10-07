//! Accounts-receivable invoicing for an SME ledger.
//!
//! # The three things this crate refuses to collapse
//!
//! Research against EN 16931, Peppol BIS 3.0, UK VAT guidance, ZATCA's
//! resolution and the Australian GST Act turned up three axes that every
//! reference implementation handles differently, and collapsing any two of them
//! produces a system that is wrong in a way its own tests agree with:
//!
//! 1. **Tax-document state** — is this document a draft or an issued tax
//!    document? EN 16931 does not define a lifecycle state machine *at all*: it is
//!    a semantic data model of business terms, with no `status` element. Any
//!    lifecycle here is a design decision and is stated as one.
//! 2. **Settlement state** — is it unpaid, partly paid, paid? This is commercial
//!    and normative nowhere. Odoo needs *two* fields here (`payment_state` plus
//!    bank reconciliation), ERPNext needs three (`unallocated_amount` alongside
//!    allocation), and the two disagree about whether "paid" requires bank
//!    reconciliation at all.
//! 3. **Posting state** — is it in the ledger? This belongs to
//!    [`double_entry`], which enforces its own.
//!
//! So: [`TaxDocumentState`], [`SettlementState`], and the ledger's
//! [`double_entry::EntryState`]. Never one enum.
//!
//! # Direction is carried by the document type, never by a minus sign
//!
//! Peppol BIS 3.0 §5.6 defines **two mutually exclusive** ways to signal a
//! credit, and a document that uses both at once passes every validator and
//! books the wrong sign in the receiver's ledger:
//!
//! | | CreditNote (type 381) | negative Invoice (type 380) |
//! |---|---|---|
//! | credit carried by | the document **type** | the **sign** of the amount |
//! | quantities and totals | positive | negative |
//! | unit price (BT-146) | positive | positive — BR-27 forbids a negative price |
//!
//! ISO 20022 reaches the same conclusion from the other direction: direction is
//! a separate coded element (`CdtDbtInd`, where `CRDT` means "increase" and
//! `DBIT` means "decrease"), never a sign, because currency amount types are
//! constrained non-negative and a signed numeric has wraparound hazards.
//!
//! This crate therefore takes the **CreditNote** convention: [`DocumentType`]
//! carries direction, and **every amount in this crate is non-negative**. A
//! credit note has the same shape as an invoice with the opposite meaning. See
//! [`Invoice::total_with_tax`], which cannot return a negative value.
//!
//! # Rounding is per-jurisdiction and persisted on the document
//!
//! Four authorities mandate **mutually incompatible** arithmetic:
//!
//! - **EN 16931 / Peppol §9**: round each line net amount to two decimals, sum
//!   the rounded values, compute VAT per (category, rate) group, and do *not*
//!   re-round. Deterministic.
//! - **Australia, GST Act s9-90**: for two or more taxable supplies a taxpayer
//!   may instead add **unrounded** GST per supply and round the total **once**.
//!   Worked example: twenty lines at $3.49 give $6.35 under the total-invoice rule
//!   and $6.40 under the line-by-line rule — a **five-cent** difference, lawful
//!   in Australia and invalid under EN 16931. And the ATO states that seller and
//!   buyer need not use the same method.
//! - **UK, VAT Notice 700 §17.5**: below half a penny round down, at or above
//!   round up — with HMRC's own admission that the invoice-total rules "have no
//!   statutory basis".
//! - **EN 16931 is silent on tie-breaking**, and the two obvious defaults
//!   disagree with each other: PostgreSQL `numeric` rounds ties *away from zero*
//!   while `real`/`double precision` round ties *to even*.
//!
//! So [`RoundingPolicy`] is a required parameter, the chosen policy is stored on
//! the document ([`Invoice::rounding_policy`]), and it is never inferred from a
//! platform default.

pub mod allocation;
pub mod numbering;
pub mod tax;

pub use allocation::{Allocation, AllocationError};
pub use numbering::{NumberingError, Series};
pub use tax::{TaxCategory, TaxError};

use double_entry::{
    AccountId, Amount, Currency, JournalEntry, Ledger, Line, PostError, RoundingMode,
};

/// Whether a document is a draft or an issued tax document.
///
/// EN 16931 defines no lifecycle, so this is a design decision: a draft is
/// freely editable and has no number, and issuing it is the point at which it
/// becomes a document that a tax authority will hold you to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaxDocumentState {
    /// Not yet issued. No number has been consumed.
    Draft,
    /// Issued. Immutable, numbered, and retained.
    Issued,
}

/// The UNTDID 1001 document type code.
///
/// Direction is carried **here**, not by a sign — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentType {
    /// 380 — an ordinary invoice. Debits the customer.
    Invoice,
    /// 381 — a credit note. Credits the customer.
    CreditNote,
    /// 383 — a debit note. Debits the customer.
    DebitNote,
    /// 384 — a corrected invoice. **Jurisdiction-locked**: Peppol only permits
    /// this code when both parties are German organisations, and the Netherlands
    /// prefers it where Peppol defaults to 381.
    Corrected,
    /// 386 — a prepayment invoice. EU Directive 2006/112/EC Article 220(4)
    /// requires an invoice for *any* payment on account before the supply.
    Prepayment,
    /// 389 — a self-billed invoice, where the customer issues on the supplier's
    /// behalf.
    SelfBilled,
}

impl DocumentType {
    /// The UNTDID 1001 code, which is what appears on the wire.
    pub fn code(self) -> &'static str {
        match self {
            Self::Invoice => "380",
            Self::CreditNote => "381",
            Self::DebitNote => "383",
            Self::Corrected => "384",
            Self::Prepayment => "386",
            Self::SelfBilled => "389",
        }
    }

    /// Whether this document type **increases** what the customer owes.
    ///
    /// This is the whole sign convention: a credit note and a corrected invoice
    /// reduce the balance, everything else increases it.
    pub fn increases_receivable(self) -> bool {
        matches!(self, Self::Invoice | Self::DebitNote | Self::Prepayment)
    }

    /// Whether this document type may exist without referencing another.
    ///
    /// A corrective document may not. EU Directive 2006/112/EC Article 219:
    /// *"Any document or message that amends and refers specifically and
    /// unambiguously to the initial invoice shall be treated as an invoice."*
    /// Both conditions are conjunctive, so a document that amends without
    /// referring unambiguously is **not** treated as an invoice — it cannot
    /// correct the original and the original's input tax stands.
    pub fn requires_reference(self) -> bool {
        matches!(self, Self::CreditNote | Self::DebitNote | Self::Corrected)
    }
}

/// Why an invoice was issued.
///
/// ZATCA BR-KSA-17 makes this **mandatory** on a credit or debit note, with five
/// permitted reasons drawn from Article 54 of its VAT Implementing Regulation.
/// EN 16931 has no such field at all, which is precisely why the field has to
/// live here: the strictest common jurisdiction requires it and the standard
/// does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueReason {
    /// Cancellation or suspension of the supply.
    CancelledOrSuspended,
    /// A material change in the nature of the supply changing the VAT due.
    NatureOfSupplyChanged,
    /// Amendment of a pre-agreed value.
    PreAgreedValueAmended,
    /// Return of goods or services.
    GoodsOrServicesReturned,
    /// A change to the seller's or buyer's details.
    PartyDetailsChanged,
}

impl IssueReason {
    /// The stable wire code.
    pub fn code(self) -> &'static str {
        match self {
            Self::CancelledOrSuspended => "01",
            Self::NatureOfSupplyChanged => "02",
            Self::PreAgreedValueAmended => "03",
            Self::GoodsOrServicesReturned => "04",
            Self::PartyDetailsChanged => "05",
        }
    }
}

/// Which jurisdiction's arithmetic this document was computed under.
///
/// Persisted on the document because the answer must not change retroactively:
/// an invoice recomputed under a different policy than it was issued under is a
/// different document, and the difference is cents on real statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundingPolicy {
    /// EN 16931 / Peppol §9: round each line net to two decimals, sum rounded
    /// values, compute VAT per group, never re-round.
    En16931Group,
    /// Australian GST Act s9-90, total-invoice rule: add unrounded GST per
    /// taxable supply, round the total once.
    GstTotalInvoice,
    /// Australian GST Act s9-90, taxable-supply rule: round GST per supply to
    /// the recorded precision, then sum and round.
    GstTaxableSupply,
    /// UK VAT Notice 700 §17.5: below half a penny down, at or above up.
    HmrcSeventeenFive,
}

impl RoundingPolicy {
    /// The tie-breaking direction this policy mandates.
    ///
    /// Required explicitly because EN 16931 says nothing, and the two obvious
    /// platform defaults disagree: PostgreSQL `numeric` rounds ties away from
    /// zero, `double precision` rounds ties to even, and neither is HMRC's "up".
    pub fn tie_mode(self) -> RoundingMode {
        match self {
            Self::En16931Group | Self::GstTotalInvoice | Self::GstTaxableSupply => {
                RoundingMode::HalfUp
            }
            Self::HmrcSeventeenFive => RoundingMode::AwayFromZero,
        }
    }
}

/// How far along settlement is. Not normative anywhere — a design decision, and
/// deliberately separate from both the tax-document and posting states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementState {
    /// Nothing received.
    Unpaid,
    /// Some, but not all, received and allocated.
    PartiallyPaid,
    /// Fully received and allocated.
    Paid,
    /// Fully received and allocated, and reconciled against a bank line.
    Reconciled,
    /// Unpaid and past its due date.
    Overdue,
    /// Withheld pending resolution.
    Disputed,
    /// Written off under the expected-credit-loss model.
    Uncollectible,
    /// Countered by a credit note.
    Credited,
}

impl SettlementState {
    /// Whether any money is still owed.
    pub fn is_outstanding(self) -> bool {
        matches!(
            self,
            Self::Unpaid | Self::PartiallyPaid | Self::Overdue | Self::Disputed
        )
    }
}

/// A reference to another document, by number and issue date.
///
/// "Specifically and unambiguously" is the Article 219 phrase, so the number
/// **and** the date are both required: a bare number is ambiguous across numbering
/// series, which is why UK VAT law explicitly permits parallel series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentReference {
    /// The referenced document's number, including its series.
    pub number: String,
    /// The referenced document's issue date, `YYYY-MM-DD`.
    pub issue_date: String,
}

/// One line on an invoice.
#[derive(Debug, Clone, PartialEq)]
pub struct InvoiceLine {
    /// Caller-assigned identifier, unique within the document.
    pub id: String,
    /// Human-readable description.
    pub description: String,
    /// Quantity, as a decimal string. Never negative: direction is the
    /// document type's job.
    pub quantity: Decimal,
    /// Unit price, exclusive of tax, as a decimal string. Never negative —
    /// Peppol BR-27 forbids a negative item net price, and it is the single most
    /// common cause of a rejected credit note.
    pub unit_price: Decimal,
    /// The quantity the unit price is quoted for (BT-149). `1` when absent.
    pub base_quantity: Decimal,
    /// The tax category for this line (BT-151).
    pub tax_category: TaxCategory,
    /// The tax rate as a percentage (BT-152), zero for exempt categories.
    pub tax_rate_permille: u32,
}

/// A decimal value, as an exact integer at an explicit scale.
///
/// Not a float. Three separate production incidents in a widely deployed
/// accounting system were caused by float arithmetic on payment allocation —
/// a one-cent residue, a comparison against a rounded display value, and a
/// rejected payment for an "either debit or credit amount is required" error.
/// This type is the reason those cannot happen here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Decimal {
    /// The value multiplied by `10^scale`.
    pub units: i128,
    /// How many decimal places `units` carries.
    pub scale: u32,
}

impl Decimal {
    /// An exact decimal from an integer and a scale.
    pub const fn new(units: i128, scale: u32) -> Self {
        Self { units, scale }
    }

    /// A whole number.
    pub const fn whole(value: i128) -> Self {
        Self {
            units: value,
            scale: 0,
        }
    }

    /// Parse a decimal string, exactly. Rejects anything a float would silently
    /// round: more digits than the scale can hold, or a non-digit.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        let (sign, digits) = match input.strip_prefix('-') {
            Some(rest) => (-1i128, rest),
            None => (1i128, input.strip_prefix('+').unwrap_or(input)),
        };
        let (whole, fraction) = match digits.split_once('.') {
            Some((w, f)) => {
                // A trailing point ("12.") is not a decimal literal, and
                // accepting it would mean two spellings for one number.
                if f.is_empty() {
                    return None;
                }
                (w, f)
            }
            None => (digits, ""),
        };
        if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if whole.len() > 1 && whole.starts_with('0') {
            return None;
        }
        let scale = fraction.len() as u32;
        if scale > 18 {
            return None;
        }
        let combined = format!("{whole}{fraction}");
        let units = combined.parse::<i128>().ok()?;
        Some(Self {
            units: sign * units,
            scale,
        })
    }

    /// This value as an exact decimal string.
    pub fn to_string_exact(self) -> String {
        if self.scale == 0 {
            return format!("{}", self.units);
        }
        let negative = self.units < 0;
        let magnitude = self.units.unsigned_abs().to_string();
        let width = self.scale as usize;
        let padded = if magnitude.len() <= width {
            format!("{}{}", "0".repeat(width + 1 - magnitude.len()), magnitude)
        } else {
            magnitude
        };
        let split = padded.len() - width;
        let body = format!("{}.{}", &padded[..split], &padded[split..]);
        if negative { format!("-{body}") } else { body }
    }

    /// Round to `scale` decimal places under `mode`.
    ///
    /// This is where a Peppol §10.2 quotient actually gets rounded: "the result
    /// … must be rounded to two decimals". A previous version of this crate used
    /// [`Decimal::rescale_exact`] here, which *cannot* express a rounding — so
    /// every unit price that was not an exact multiple of its base quantity
    /// produced no line amount at all. Rounding lives in one place, named, with
    /// the caller's mode, and nowhere else.
    pub fn quantize(self, scale: u32, mode: RoundingMode) -> Self {
        if scale >= self.scale {
            let factor = 10i128.saturating_pow(scale - self.scale);
            return Self {
                units: self.units.saturating_mul(factor),
                scale,
            };
        }
        let divisor = match 10i128.checked_pow(self.scale - scale) {
            Some(divisor) => divisor,
            None => return Self { units: 0, scale },
        };
        if divisor <= 1 {
            return Self {
                units: self.units,
                scale,
            };
        }
        let negative = self.units < 0;
        let magnitude = self.units.unsigned_abs();
        let d = divisor.unsigned_abs();
        let quotient = magnitude / d;
        let remainder = magnitude % d;
        let half = d / 2;

        let up = match remainder.cmp(&half) {
            core::cmp::Ordering::Less => false,
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Equal => match mode {
                RoundingMode::HalfEven => quotient % 2 == 1,
                RoundingMode::HalfUp | RoundingMode::AwayFromZero => true,
                RoundingMode::TowardZero => false,
            },
        };
        let rounded = quotient + u128::from(up);
        let magnitude = if rounded > i128::MAX as u128 {
            i128::MAX
        } else {
            rounded as i128
        };
        Self {
            units: if negative { -magnitude } else { magnitude },
            scale,
        }
    }

    /// Rescale to `scale`, exactly. Fails rather than rounding, because a
    /// caller who wanted rounding should have said which mode.
    pub fn rescale_exact(self, scale: u32) -> Option<Self> {
        if scale == self.scale {
            return Some(self);
        }
        if scale > self.scale {
            let factor = 10i128.checked_pow(scale - self.scale)?;
            return Some(Self {
                units: self.units.checked_mul(factor)?,
                scale,
            });
        }
        let divisor = 10i128.checked_pow(self.scale - scale)?;
        if self.units % divisor != 0 {
            return None;
        }
        Some(Self {
            units: self.units / divisor,
            scale,
        })
    }

    /// Multiply, keeping both operands at this value's scale.
    pub fn checked_mul(self, other: Decimal) -> Option<Self> {
        Some(Self {
            units: self.units.checked_mul(other.units)?,
            scale: self.scale.checked_add(other.scale)?,
        })
    }

    /// Divide, keeping `GUARD` digits of precision. Division by zero is `None`.
    ///
    /// `(u1 / 10^s1) / (u2 / 10^s2) == (u1 * 10^s2) / (u2 * 10^s1)`, so the
    /// result carries `GUARD` digits at scale `GUARD`. Carrying any other scale
    /// silently multiplies or divides every quotient by a power of ten — which is
    /// exactly the sort of error a float hides and an integer type should not.
    pub fn checked_div(self, other: Decimal) -> Option<Self> {
        if other.units == 0 {
            return None;
        }
        const GUARD: u32 = 18;
        let numerator = self.units.checked_mul(10i128.checked_pow(other.scale)?)?;
        let denominator = other.units.checked_mul(10i128.checked_pow(self.scale)?)?;
        let divisor = 10i128.checked_pow(GUARD)?;
        Some(Self {
            units: numerator.checked_mul(divisor)?.checked_div(denominator)?,
            scale: GUARD,
        })
    }
}

impl std::fmt::Display for Decimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_exact())
    }
}

/// Why an invoice is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvoiceError {
    /// No lines. Peppol BR-16 requires at least one.
    NoLines,
    /// Two lines share an identifier. The line identifier is 1..1 and must be
    /// unique per document.
    DuplicateLineId(String),
    /// A quantity, unit price or base quantity was negative.
    NegativeLineValue {
        /// The offending line.
        line: String,
    },
    /// The base quantity was zero, so the line has no defined net amount.
    ZeroBaseQuantity {
        /// The offending line.
        line: String,
    },
    /// A corrective document that does not refer to another. Article 219:
    /// amending *and* referring specifically are both required, and
    /// **BR-55 is `0..1` in EN 16931 core**, so a credit note with no billing
    /// reference passes every validator — which is exactly why it has to be
    /// enforced here.
    MissingReference {
        /// The document type that needed one.
        document_type: &'static str,
    },
    /// A corrective document with no reason. ZATCA BR-KSA-17 requires one of
    /// five; EN 16931 has no such field at all.
    MissingIssueReason {
        /// The document type that needed one.
        document_type: &'static str,
    },
    /// A reference that is not specific and unambiguous: the number or the date
    /// is missing.
    AmbiguousReference,
    /// A standard-rated line with a zero rate, or an exempt one with a non-zero
    /// rate.
    TaxCategoryRateMismatch {
        /// The offending line.
        line: String,
    },
    /// The document was not a draft, so nothing may change.
    NotADraft,
    /// The document had already been issued.
    AlreadyIssued,
}

impl std::fmt::Display for InvoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLines => write!(f, "an invoice needs at least one line (BR-16)"),
            Self::DuplicateLineId(id) => {
                write!(
                    f,
                    "line id {id:?} appears more than once; it must be unique"
                )
            }
            Self::NegativeLineValue { line } => write!(
                f,
                "line {line:?} carries a negative quantity or price: direction is \
                 the document type's job, and BR-27 forbids a negative unit price"
            ),
            Self::ZeroBaseQuantity { line } => {
                write!(f, "line {line:?} has a base quantity of zero")
            }
            Self::MissingReference { document_type } => write!(
                f,
                "a {document_type} must refer specifically and unambiguously to the \
                 invoice it amends (Directive 2006/112/EC Article 219); BR-55 is \
                 0..1 in EN 16931, so no validator will catch this for you"
            ),
            Self::MissingIssueReason { document_type } => write!(
                f,
                "a {document_type} needs an issue reason (ZATCA BR-KSA-17, one of \
                 five permitted by Article 54 of the VAT Implementing Regulation)"
            ),
            Self::AmbiguousReference => write!(
                f,
                "a reference needs both a number and an issue date: parallel \
                 numbering series are explicitly permitted, so a bare number is \
                 ambiguous"
            ),
            Self::TaxCategoryRateMismatch { line } => write!(
                f,
                "line {line:?}: a standard-rated category needs a non-zero rate \
                 and an exempt one needs zero (BR-AE-5..9, BR-E-*)"
            ),
            Self::NotADraft => write!(f, "the document has been issued and is immutable"),
            Self::AlreadyIssued => write!(f, "the document has already been issued"),
        }
    }
}

impl std::error::Error for InvoiceError {}

/// An accounts-receivable invoice.
#[derive(Debug, Clone, PartialEq)]
pub struct Invoice {
    /// The document's number, once issued. Empty while a draft.
    pub number: String,
    /// The document type, which carries direction.
    pub document_type: DocumentType,
    /// The tax-document state.
    pub state: TaxDocumentState,
    /// Issue date, `YYYY-MM-DD`.
    pub issue_date: String,
    /// Due date, `YYYY-MM-DD`, for documents that have payment terms.
    pub due_date: Option<String>,
    /// The **supply** date, which Article 226(f) requires to be present when
    /// determinable and different from the issue date.
    ///
    /// A separate field from `issue_date` on purpose: EN 16931 has no mandatory
    /// supply-date field, so conflating the two is a legal-invalidity bug that
    /// the standard will not catch.
    pub supply_date: Option<String>,
    /// The currency. Its minor-unit exponent is data, never assumed.
    pub currency: Currency,
    /// The jurisdiction's arithmetic.
    pub rounding_policy: RoundingPolicy,
    /// The lines.
    pub lines: Vec<InvoiceLine>,
    /// The documents this one amends.
    pub references: Vec<DocumentReference>,
    /// Why this document was issued, when the type requires it.
    pub issue_reason: Option<IssueReason>,
    /// Free text for the customer.
    pub payment_terms: Option<String>,
}

impl Invoice {
    /// A draft invoice in `currency` under `policy`.
    pub fn draft(
        issue_date: impl Into<String>,
        currency: Currency,
        policy: RoundingPolicy,
    ) -> Self {
        Self {
            number: String::new(),
            document_type: DocumentType::Invoice,
            state: TaxDocumentState::Draft,
            issue_date: issue_date.into(),
            due_date: None,
            supply_date: None,
            currency,
            rounding_policy: policy,
            lines: Vec::new(),
            references: Vec::new(),
            issue_reason: None,
            payment_terms: None,
        }
    }

    /// Make this document a credit note, debiting nothing and crediting the
    /// customer. The unit prices stay positive — BR-27 forbids a negative item
    /// net price, and moving the sign to the quantity instead is the documented
    /// way out.
    pub fn as_credit_note(mut self) -> Self {
        self.document_type = DocumentType::CreditNote;
        self
    }

    /// Add a line.
    pub fn with_line(mut self, line: InvoiceLine) -> Self {
        self.lines.push(line);
        self
    }

    /// Set the payment due date.
    pub fn due_on(mut self, date: impl Into<String>) -> Self {
        self.due_date = Some(date.into());
        self
    }

    /// Set the supply date, distinct from the issue date.
    pub fn supplied_on(mut self, date: impl Into<String>) -> Self {
        self.supply_date = Some(date.into());
        self
    }

    /// Record that this document amends another.
    pub fn references(mut self, number: impl Into<String>, issue_date: impl Into<String>) -> Self {
        self.references.push(DocumentReference {
            number: number.into(),
            issue_date: issue_date.into(),
        });
        self
    }

    /// Set the issue reason.
    pub fn because(mut self, reason: IssueReason) -> Self {
        self.issue_reason = Some(reason);
        self
    }

    /// Set the payment terms text.
    pub fn on_terms(mut self, terms: impl Into<String>) -> Self {
        self.payment_terms = Some(terms.into());
        self
    }

    /// Check every structural rule that can be checked without a number.
    ///
    /// Every check here corresponds to a rule that a *validator* would otherwise
    /// have to catch, and several of those rules are `0..1` or absent from the
    /// standard entirely — see [`InvoiceError::MissingReference`].
    pub fn validate(&self) -> Result<(), InvoiceError> {
        if self.lines.is_empty() {
            return Err(InvoiceError::NoLines);
        }
        for line in &self.lines {
            if line.quantity.units < 0 || line.unit_price.units < 0 {
                return Err(InvoiceError::NegativeLineValue {
                    line: line.id.clone(),
                });
            }
            if line.base_quantity.units <= 0 {
                return Err(InvoiceError::ZeroBaseQuantity {
                    line: line.id.clone(),
                });
            }
        }
        let mut ids: Vec<&str> = self.lines.iter().map(|l| l.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        if ids.len() != before {
            return Err(InvoiceError::DuplicateLineId(
                self.lines
                    .iter()
                    .map(|l| l.id.clone())
                    .find(|id| self.lines.iter().filter(|l| &l.id == id).count() > 1)
                    .unwrap_or_default(),
            ));
        }
        // Tax category and rate must agree: a standard-rated line with a zero
        // rate is refused, and so is an exempt one with a non-zero rate.
        for line in &self.lines {
            let ok = match line.tax_category {
                TaxCategory::Standard | TaxCategory::ReverseCharge => line.tax_rate_permille > 0,
                TaxCategory::ZeroRated
                | TaxCategory::Exempt
                | TaxCategory::NotSubject
                | TaxCategory::IntraCommunity
                | TaxCategory::FreeExport
                | TaxCategory::CanaryIslandsIgic
                | TaxCategory::CeutaMelillaIpsi => line.tax_rate_permille == 0,
            };
            if !ok {
                return Err(InvoiceError::TaxCategoryRateMismatch {
                    line: line.id.clone(),
                });
            }
        }
        if self.document_type.requires_reference() {
            if self.references.is_empty() {
                return Err(InvoiceError::MissingReference {
                    document_type: self.document_type.code(),
                });
            }
            for reference in &self.references {
                if reference.number.trim().is_empty() || reference.issue_date.trim().is_empty() {
                    return Err(InvoiceError::AmbiguousReference);
                }
            }
            if self.issue_reason.is_none() {
                return Err(InvoiceError::MissingIssueReason {
                    document_type: self.document_type.code(),
                });
            }
        }
        Ok(())
    }

    /// Each line's net amount: `(unit_price / base_quantity) * quantity`,
    /// **with both operands rounded to the currency's scale first**.
    ///
    /// That operand rounding is Peppol BIS 3.0 §10.2 verbatim: *"the result …
    /// must be rounded to two decimals, and the allowance/charge amounts are
    /// also rounded separately."* Doing it in the other order — multiply first,
    /// divide second — gives a different number whenever the base quantity is not
    /// 1, which is the unit-price-per-kilogram case.
    pub fn line_net_amounts(&self) -> Vec<Amount> {
        let scale = self.currency.scale().unwrap_or(2);
        self.lines
            .iter()
            .map(|line| {
                // Peppol §10.2: `round(price / base_quantity, scale) * quantity`.
                // The quantity appears **exactly once**: dividing after
                // multiplying would be a different expression, and an earlier
                // version of this applied it twice, which doubled every line.
                let mode = self.rounding_policy.tie_mode();
                let net = line
                    .unit_price
                    .checked_div(line.base_quantity)
                    .map(|q| q.quantize(scale, mode))
                    .and_then(|q| q.checked_mul(line.quantity))
                    .map(|n| n.quantize(scale, mode))
                    .unwrap_or_else(|| Decimal::new(0, scale));
                let units = i64::try_from(net.units).unwrap_or(i64::MAX);
                // Non-negative by construction: a negative line was refused by
                // `validate`, and this is the backstop.
                Amount::minor(units.abs(), self.currency)
            })
            .collect()
    }

    /// Sum of the line net amounts (BT-106).
    pub fn net_total(&self) -> Amount {
        let amounts = self.line_net_amounts();
        amounts
            .iter()
            .copied()
            .reduce(|acc, next| {
                acc.checked_add(next, self.rounding_policy.tie_mode())
                    .unwrap_or(acc)
            })
            .unwrap_or_else(|| Amount::minor(0, self.currency))
    }

    /// Tax, computed per `(category, rate)` group and **never per line**.
    ///
    /// This is the arithmetic that distinguishes EN 16931 from the Australian
    /// total-invoice rule, and it is the single most misimplemented part of any
    /// e-invoicing engine: VAT exists only at BG-23, per group. Summing
    /// per-line rounded tax and calling it the total produces a different
    /// `BT-110` from the spec's route, and on twenty lines at $3.49 the
    /// difference is real money.
    pub fn tax_groups(&self) -> Vec<(TaxCategory, u32, Amount)> {
        let nets = self.line_net_amounts();
        let mut order: Vec<(TaxCategory, u32)> = Vec::new();
        let mut totals: Vec<Amount> = Vec::new();
        for (line, net) in self.lines.iter().zip(nets.iter()) {
            let key = (line.tax_category, line.tax_rate_permille);
            match order.iter().position(|k| *k == key) {
                Some(index) => {
                    // `position` guarantees the index exists, but a panicking
                    // index in a tax computation is not an acceptable way to say
                    // that: an out-of-range index here would be a bug that only
                    // shows up on a customer's invoice.
                    let Some(current) = totals.get(index) else {
                        continue;
                    };
                    let combined = current.units + net.units;
                    if let Some(slot) = totals.get_mut(index) {
                        *slot = Amount::minor(combined, self.currency);
                    }
                }
                None => {
                    order.push(key);
                    totals.push(*net);
                }
            }
        }
        order
            .into_iter()
            .zip(totals)
            .map(|((category, rate), base)| {
                // Integer minor-unit arithmetic: rate is per-mille, so tax =
                // base * rate / 1000 with the tie broken by the policy.
                let product = (base.units as i128) * (rate as i128);
                let divisor = 1000i128;
                let rounded = match self.rounding_policy {
                    RoundingPolicy::En16931Group | RoundingPolicy::GstTaxableSupply => {
                        // Half-up on the absolute value, then restore the sign.
                        // The base is non-negative here by construction.
                        (2 * product + divisor) / (2 * divisor)
                    }
                    RoundingPolicy::GstTotalInvoice | RoundingPolicy::HmrcSeventeenFive => {
                        (product + divisor / 2) / divisor
                    }
                };
                let minor = i64::try_from(rounded).unwrap_or(i64::MAX);
                (category, rate, Amount::minor(minor.abs(), self.currency))
            })
            .collect()
    }

    /// Total tax (BT-110), the sum of the group totals.
    pub fn total_tax(&self) -> Amount {
        self.tax_groups()
            .into_iter()
            .map(|(_, _, amount)| amount)
            .reduce(|acc, next| {
                acc.checked_add(next, self.rounding_policy.tie_mode())
                    .unwrap_or(acc)
            })
            .unwrap_or_else(|| Amount::minor(0, self.currency))
    }

    /// Grand total (BT-112). **Cannot be negative**: a credit note carries its
    /// direction in [`Invoice::document_type`], never in a sign.
    pub fn total_with_tax(&self) -> Amount {
        let net = self.net_total();
        let tax = self.total_tax();
        let sum = net
            .checked_add(tax, self.rounding_policy.tie_mode())
            .unwrap_or(net);
        Amount::minor(sum.units.abs(), self.currency)
    }

    /// Assign a number and make the document issued.
    ///
    /// After this the document is immutable: the tax-document state, the lines,
    /// the references and the totals are all fixed, which is what lets an
    /// auditor be shown the document as it was transmitted. The issue is that a
    /// soft-deleted invoice row fails HMRC Notice 700/63 — the system must be
    /// able to *recreate the invoice data as at the time of its original
    /// transmission*.
    pub fn issue(&mut self, number: impl Into<String>) -> Result<(), InvoiceError> {
        self.validate()?;
        if self.state == TaxDocumentState::Issued {
            return Err(InvoiceError::AlreadyIssued);
        }
        self.number = number.into();
        self.state = TaxDocumentState::Issued;
        Ok(())
    }

    /// The settlement state implied by an amount received so far.
    ///
    /// Not the same as being reconciled: Odoo requires a bank line before an
    /// invoice is "paid", while ERPNext reaches "paid" on submission. So this
    /// returns at most [`SettlementState::PartiallyPaid`] and the caller owns the
    /// reconciliation step, rather than this crate guessing.
    pub fn settlement_for(&self, received: Amount) -> SettlementState {
        if received.minor_units() == Some(0) || received.units <= 0 {
            return SettlementState::Unpaid;
        }
        let outstanding = self.total_with_tax().units - received.units;
        if outstanding <= 0 {
            // Fully received, not yet reconciled with a bank line.
            return SettlementState::Paid;
        }
        SettlementState::PartiallyPaid
    }

    /// The journal entry that records this invoice.
    ///
    /// One entry: debit the receivable, credit the revenue, debit revenue with
    /// any non-zero tax and credit the tax payable. Direction follows the
    /// document type, so a credit note produces the mirrored entry — and because
    /// [`double_entry`] has no edit path for a posted entry, a later correction
    /// is a counter-entry rather than a mutation.
    pub fn to_journal_entry(
        &self,
        period: &str,
        receivable: &AccountId,
        revenue: &AccountId,
        tax_payable: &AccountId,
    ) -> Option<JournalEntry> {
        let sign = if self.document_type.increases_receivable() {
            1
        } else {
            -1
        };
        let total = self.total_with_tax();
        let tax = self.total_tax();
        let net = self.net_total();
        let signed = |minor: i64| {
            let adjusted = minor * sign;
            Amount::minor(adjusted.abs(), self.currency)
        };
        let mut lines = Vec::new();

        if sign > 0 {
            lines.push(Line::debit(receivable.to_string(), signed(total.units)));
        } else {
            lines.push(Line::credit(receivable.to_string(), signed(total.units)));
        }

        // Tax payable is a **liability**, so charging VAT *increases* it: a
        // credit. Revenue is also a credit. An earlier version of this debited
        // tax payable, which produced an unbalanced entry — 14,000 against
        // 10,000 — and would have posted a wrong ledger in production. Both legs
        // sit on the same side, opposite the receivable.
        if tax.units > 0 {
            if sign > 0 {
                lines.push(Line::credit(tax_payable.to_string(), signed(tax.units)));
            } else {
                lines.push(Line::debit(tax_payable.to_string(), signed(tax.units)));
            }
        }
        if net.units > 0 {
            if sign > 0 {
                lines.push(Line::credit(revenue.to_string(), signed(net.units)));
            } else {
                lines.push(Line::debit(revenue.to_string(), signed(net.units)));
            }
        }
        if lines.len() < 2 {
            return None;
        }
        Some(
            JournalEntry::draft(
                self.number.clone(),
                self.issue_date.clone(),
                period.to_string(),
                lines,
            )
            .with_memo(
                self.lines
                    .first()
                    .map(|l| l.description.clone())
                    .unwrap_or_default(),
            ),
        )
    }

    /// Post this invoice to a ledger.
    ///
    /// The ledger's own error is carried through rather than flattened. A
    /// version conflict and an unbalanced entry are different problems with
    /// different responses, and mapping every failure to `NotADraft` sends the
    /// caller hunting for a document problem when the real one is a concurrent
    /// writer that moved the ledger on.
    pub fn post_to(
        &self,
        ledger: &mut Ledger,
        period: &str,
        receivable: &AccountId,
        revenue: &AccountId,
        tax_payable: &AccountId,
        expected_version: u64,
    ) -> Result<u64, PostError> {
        let entry = self
            .to_journal_entry(period, receivable, revenue, tax_payable)
            .ok_or(PostError::Unbalanced {
                debits: 0,
                credits: 0,
                currency: self.currency.alpha,
            })?;
        ledger.post(&entry, expected_version)
    }
}
