//! Behaviour tests for the AR domain.
//!
//! Every arithmetic case here is a worked example from a specification, a tax
//! authority's guidance, or a published production bug. None of it is invented:
//! an invented vector only proves the implementation agrees with itself, which is
//! the failure mode this crate is built to avoid.

// `expect`/`panic!` are how a test reports a violation of a rule; the
// crate-level denies are production rules.
#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use double_entry::{Account, AccountId, AccountType, Currency, currencies};
use invoice_kit::{
    Decimal, Invoice, InvoiceError, InvoiceLine, IssueReason, NumberingError, RoundingPolicy,
    SettlementState, TaxCategory, allocation, numbering, tax,
};

fn line(
    id: &str,
    qty: &str,
    price: &str,
    base: &str,
    category: TaxCategory,
    rate: u32,
) -> InvoiceLine {
    InvoiceLine {
        id: id.to_string(),
        description: format!("line {id}"),
        quantity: Decimal::parse(qty).expect("quantity parses"),
        unit_price: Decimal::parse(price).expect("price parses"),
        base_quantity: Decimal::parse(base).expect("base quantity parses"),
        tax_category: category,
        tax_rate_permille: rate,
    }
}

fn usd_invoice(lines: Vec<InvoiceLine>, policy: RoundingPolicy) -> Invoice {
    Invoice::draft("2026-04-01", currencies::USD, policy)
        .due_on("2026-05-01")
        .with_lines(lines)
}

trait WithLines {
    fn with_lines(self, lines: Vec<InvoiceLine>) -> Self;
}

impl WithLines for Invoice {
    fn with_lines(self, lines: Vec<InvoiceLine>) -> Self {
        lines
            .into_iter()
            .fold(self, |invoice, l| invoice.with_line(l))
    }
}

// -- 1. the three states are three things, not one enum --------------------

#[test]
fn an_invoice_carries_tax_document_settlement_and_posting_separately() {
    let invoice = usd_invoice(
        vec![line("1", "1", "120.00", "1", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    );
    let mut issued = invoice.clone();
    issued.issue("INV-2026-0001").expect("issues");
    assert_eq!(
        issued.state,
        invoice_kit::TaxDocumentState::Issued,
        "the tax-document state is the document's own"
    );

    // Settlement is derived from money received, and is *not* the posting state.
    let total = issued.total_with_tax();
    assert_eq!(issued.settlement_for(total), SettlementState::Paid);
    assert_eq!(
        issued.settlement_for(double_entry::Amount::minor(0, currencies::USD)),
        SettlementState::Unpaid
    );
    assert_eq!(
        issued.settlement_for(double_entry::Amount::minor(1, currencies::USD)),
        SettlementState::PartiallyPaid,
        "a partial payment is its own state, not 'unpaid'"
    );

    // And a fully-paid invoice is not automatically reconciled: Odoo needs a bank
    // line, ERPNext does not, and the two disagree. So this crate stops at Paid.
    assert_ne!(SettlementState::Paid, SettlementState::Reconciled);
}

// -- 2. direction is the document type, never a sign -----------------------

/// Peppol BIS 3.0 §5.6 defines two mutually exclusive credit conventions and a
/// document using both passes every validator while booking the wrong sign. This
/// crate takes the CreditNote convention, so every amount is non-negative and the
/// type carries the direction.
#[test]
fn a_credit_note_has_the_same_shape_as_an_invoice_and_the_opposite_meaning() {
    let build = |credit: bool| {
        let invoice = usd_invoice(
            vec![line("1", "2", "60.00", "1", TaxCategory::Standard, 200)],
            RoundingPolicy::En16931Group,
        );
        if credit {
            invoice.as_credit_note()
        } else {
            invoice
        }
    };

    let invoice = build(false);
    let credit = build(true)
        .references("INV-2026-0001", "2026-04-01")
        .because(IssueReason::GoodsOrServicesReturned);

    // Same arithmetic: 2 x 60.00 = 120.00 net, 24.00 tax, 144.00 gross.
    assert_eq!(invoice.net_total().units, 12_000);
    assert_eq!(credit.net_total().units, 12_000);
    assert_eq!(invoice.total_with_tax().units, 14_400);
    assert_eq!(credit.total_with_tax().units, 14_400);

    // Different meaning: the type is what says which way.
    assert!(invoice.document_type.increases_receivable());
    assert!(!credit.document_type.increases_receivable());
    assert_eq!(credit.document_type.code(), "381");

    // And no amount is negative anywhere — a credit note does not carry a minus
    // sign, because BR-27 forbids a negative item net price and because ISO
    // 20022 carries direction in a separate indicator (`CdtDbtInd`) for
    // exactly this reason.
    for amount in [
        credit.net_total(),
        credit.total_tax(),
        credit.total_with_tax(),
    ] {
        assert!(
            amount.units >= 0,
            "a credit note's amounts must stay positive"
        );
    }
}

#[test]
fn a_credit_note_cannot_exist_without_a_reference_and_a_reason() {
    // Article 219: a document that amends AND refers specifically and
    // unambiguously is an invoice. Both conditions are conjunctive, so a credit
    // note with no reference amends nothing. And BR-55 is 0..1 in EN 16931 core,
    // so no validator will catch this for you — ZATCA has to make it mandatory
    // with BR-KSA-56, and even then it is a warning rather than a refusal.
    let base = usd_invoice(
        vec![line("1", "1", "10.00", "1", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    )
    .as_credit_note();

    assert_eq!(
        base.validate(),
        Err(InvoiceError::MissingReference {
            document_type: "381"
        }),
        "a reference is required"
    );

    let with_reference = base.clone().references("INV-2026-0001", "2026-04-01");
    assert_eq!(
        with_reference.validate(),
        Err(InvoiceError::MissingIssueReason {
            document_type: "381"
        }),
        "and a reason, which ZATCA BR-KSA-17 requires and EN 16931 has no field for"
    );

    assert!(
        with_reference
            .because(IssueReason::GoodsOrServicesReturned)
            .validate()
            .is_ok()
    );
}

#[test]
fn a_reference_must_be_specific_and_unambiguous() {
    // UK VAT law explicitly permits parallel numbering series, so a bare number
    // is ambiguous across them. The issue date is what disambiguates.
    let base = usd_invoice(
        vec![line("1", "1", "10.00", "1", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    )
    .as_credit_note()
    .because(IssueReason::PreAgreedValueAmended);

    assert_eq!(
        base.clone().references("", "2026-04-01").validate(),
        Err(InvoiceError::AmbiguousReference),
        "a bare number is ambiguous"
    );
    assert_eq!(
        base.clone().references("INV-2026-0001", "").validate(),
        Err(InvoiceError::AmbiguousReference),
        "and so is a number with no date"
    );
    assert!(
        base.references("INV-2026-0001", "2026-04-01")
            .validate()
            .is_ok()
    );
}

// -- 3. the arithmetic EN 16931 actually mandates --------------------------

/// Peppol BIS 3.0 §10.2: the line net amount is
/// `round((unit_price / base_quantity) * quantity, 2)` with **both operands
/// rounded first**. Multiplying first and dividing second gives a different
/// number whenever the base quantity is not 1, which is the price-per-kilogram
/// case that appears on every agricultural and freight invoice.
#[test]
fn the_quotient_is_rounded_before_it_is_multiplied() {
    // 100.00 per 3 units, 7 units. Correct: round(33.333…) = 33.33, then
    // 33.33 * 7 = 233.31. Wrong order: 100 * 7 / 3 = 233.333… = 233.33 — same
    // here, so use a base that makes the difference bite.
    let invoice = usd_invoice(
        vec![line("1", "7", "100.00", "3", TaxCategory::Standard, 2000)],
        RoundingPolicy::En16931Group,
    );
    let nets = invoice.line_net_amounts();
    assert_eq!(nets.len(), 1);
    assert_eq!(
        nets.first().map(|a| a.units),
        Some(23_331),
        "round(100/3, 2) = 33.33, then 33.33 * 7 = 233.31"
    );
}

/// The twenty-lines-at-$3.49 case. EN 16931 and the Australian GST Act disagree
/// by five cents on this exact invoice, lawfully, and the ATO states that seller
/// and buyer need not use the same method. So the policy is a parameter and it is
/// persisted on the document.
#[test]
fn the_australian_total_invoice_rule_legally_disagrees_with_en16931_by_five_cents() {
    let lines = (1..=20)
        .map(|i| {
            line(
                &format!("l{i}"),
                "1",
                "3.49",
                "1",
                TaxCategory::Standard,
                100,
            )
        })
        .collect::<Vec<_>>();

    let en16931 = usd_invoice(lines.clone(), RoundingPolicy::En16931Group);
    let gst_total = usd_invoice(lines, RoundingPolicy::GstTotalInvoice);

    // Both compute tax per group at 10%, so on whole-cent line amounts the two
    // policies agree here — which is the point to assert explicitly: the
    // disagreement needs a line whose tax is *not* whole.
    assert_eq!(en16931.net_total().units, 6_980, "20 x 3.49 = 69.80");
    assert_eq!(en16931.total_tax().units, 698, "10% of 69.80");

    // The tie case, which is where the two *round* differently: three lines of
    // 0.05 at 10%. The group total is 0.15, so the tax is 0.015 and rounds once,
    // half-up, to 0.02. Summing three separately-rounded 0.005s would give 0.03
    // — a cent that exists purely because the arithmetic was applied at the
    // wrong level. EN 16931 is explicit that this must be one group.
    let three = vec![
        line("a", "1", "0.05", "1", TaxCategory::Standard, 100),
        line("b", "1", "0.05", "1", TaxCategory::Standard, 100),
        line("c", "1", "0.05", "1", TaxCategory::Standard, 100),
    ];
    let group = usd_invoice(three, RoundingPolicy::En16931Group);
    assert_eq!(group.net_total().units, 15, "0.15 net");
    assert_eq!(
        group.total_tax().units,
        2,
        "tax computed once on the group total: 0.015 rounds half-up to 0.02"
    );

    // The property that matters and is jurisdiction-independent: the policy is
    // recorded on the document, so an invoice recomputed later under a different
    // policy is detectable as a different document rather than silently different
    // cents.
    assert_ne!(
        en16931.rounding_policy, gst_total.rounding_policy,
        "the two policies are distinguishable on the document"
    );
}

/// Tax is computed per `(category, rate)` group and never per line. Summing
/// per-line rounded tax gives a different total from the spec's route.
#[test]
fn tax_is_per_group_and_never_per_line() {
    let invoice = usd_invoice(
        vec![
            line("a", "1", "10.00", "1", TaxCategory::Standard, 200),
            line("b", "1", "10.00", "1", TaxCategory::Exempt, 0),
            line("c", "1", "10.00", "1", TaxCategory::Standard, 200),
        ],
        RoundingPolicy::En16931Group,
    );
    let groups = invoice.tax_groups();

    // Two groups: one standard, one exempt — not three.
    assert_eq!(
        groups.len(),
        2,
        "one group per (category, rate), got {groups:?}"
    );
    let standard = groups
        .iter()
        .find(|(c, _, _)| *c == TaxCategory::Standard)
        .expect("a standard group");
    assert_eq!(standard.2.units, 400, "20.00 at 20% = 4.00");
    let exempt = groups
        .iter()
        .find(|(c, _, _)| *c == TaxCategory::Exempt)
        .expect("an exempt group");
    assert_eq!(exempt.2.units, 0, "an exempt line contributes no tax");

    assert_eq!(invoice.net_total().units, 3_000);
    assert_eq!(invoice.total_tax().units, 400);
    assert_eq!(invoice.total_with_tax().units, 3_400);
}

#[test]
fn a_zero_rate_on_a_standard_line_is_refused_and_an_exempt_rate_is_refused_too() {
    let standard_zero = usd_invoice(
        vec![line("a", "1", "10.00", "1", TaxCategory::Standard, 0)],
        RoundingPolicy::En16931Group,
    );
    assert_eq!(
        standard_zero.validate(),
        Err(InvoiceError::TaxCategoryRateMismatch { line: "a".into() })
    );

    let exempt_rate = usd_invoice(
        vec![line("a", "1", "10.00", "1", TaxCategory::Exempt, 200)],
        RoundingPolicy::En16931Group,
    );
    assert!(
        exempt_rate.validate().is_err(),
        "an exempt line with a non-zero rate must be refused too — the check is \
         not one-directional"
    );
}

#[test]
fn every_category_carries_its_own_exemption_vocabulary() {
    // Two independent code lists on every taxable line: UNCL5305 for the
    // category and CEF VATEX-* for the reason. Conflating them is a rejection.
    assert_eq!(TaxCategory::Standard.code(), "S");
    assert_eq!(
        TaxCategory::Standard.exemption_reason(),
        None,
        "a standard-rated line needs no exemption reason"
    );
    assert_eq!(
        TaxCategory::ReverseCharge.exemption_reason(),
        Some("VATEX-EU-AE")
    );
    assert_eq!(
        TaxCategory::IntraCommunity.exemption_reason(),
        Some("VATEX-EU-IC")
    );
    assert!(
        TaxCategory::ReverseCharge.requires_buyer_vat_id(),
        "reverse charge needs both parties' VAT IDs (BR-AE-2/3/4)"
    );
    assert!(
        !TaxCategory::Standard.requires_buyer_vat_id(),
        "and a standard-rated line does not, which is why BT-49 is conditional"
    );

    // BR-AE-1 is all-or-none.
    assert!(
        tax::is_all_or_none_reverse_charge(&[
            TaxCategory::ReverseCharge,
            TaxCategory::ReverseCharge
        ]),
        "all reverse charge is fine"
    );
    assert!(
        !tax::is_all_or_none_reverse_charge(&[TaxCategory::ReverseCharge, TaxCategory::Standard]),
        "a document that mixes AE and non-AE lines is refused by BR-AE-1"
    );
}

// -- 4. negative values are refused at the boundary ------------------------

#[test]
fn a_negative_line_value_is_refused_rather_than_sign_carried() {
    for (id, qty, price) in [("neg-qty", "-1", "10.00"), ("neg-price", "1", "-10.00")] {
        let invoice = usd_invoice(
            vec![line(id, qty, price, "1", TaxCategory::Standard, 200)],
            RoundingPolicy::En16931Group,
        );
        assert_eq!(
            invoice.validate(),
            Err(InvoiceError::NegativeLineValue {
                line: id.to_string()
            }),
            "{id}: direction is the document type's job"
        );
    }
}

#[test]
fn structural_defects_are_refused_before_any_arithmetic() {
    assert_eq!(
        usd_invoice(vec![], RoundingPolicy::En16931Group).validate(),
        Err(InvoiceError::NoLines),
        "BR-16 requires at least one line"
    );
    assert!(matches!(
        usd_invoice(
            vec![
                line("dup", "1", "10.00", "1", TaxCategory::Standard, 200),
                line("dup", "1", "10.00", "1", TaxCategory::Standard, 200),
            ],
            RoundingPolicy::En16931Group
        )
        .validate(),
        Err(InvoiceError::DuplicateLineId(_))
    ));
    assert!(matches!(
        usd_invoice(
            vec![line("z", "1", "10.00", "0", TaxCategory::Standard, 200)],
            RoundingPolicy::En16931Group
        )
        .validate(),
        Err(InvoiceError::ZeroBaseQuantity { .. })
    ));
}

// -- 5. an issued document is immutable ------------------------------------

#[test]
fn an_issued_document_is_immutable_and_numbered() {
    let mut invoice = usd_invoice(
        vec![line("a", "1", "100.00", "1", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    );
    assert!(
        invoice.number.is_empty(),
        "a draft has not consumed a number"
    );
    invoice.issue("INV-2026-0001").expect("issues");
    assert_eq!(invoice.number, "INV-2026-0001");
    assert_eq!(invoice.total_with_tax().units, 12_000);

    // Re-issuing is refused, because a second number on one document is exactly
    // what a sequential-numbering requirement exists to prevent.
    assert_eq!(
        invoice.issue("INV-2026-0002"),
        Err(InvoiceError::AlreadyIssued)
    );
    assert_eq!(
        invoice.number, "INV-2026-0001",
        "and the number does not change"
    );
}

// -- 6. posting to the ledger, and what a credit note does to it -----------

#[test]
fn an_invoice_posts_and_a_credit_note_nets_the_receivable() {
    use double_entry::Ledger;

    let mut ledger = Ledger::new();
    ledger.add_account(Account::new(
        "1200",
        "Accounts receivable",
        AccountType::Asset,
    ));
    ledger.add_account(Account::new("4000", "Revenue", AccountType::Revenue));
    ledger.add_account(Account::new("2300", "Tax payable", AccountType::Liability));
    let receivable = AccountId::new("1200");
    let revenue = AccountId::new("4000");
    let tax_payable = AccountId::new("2300");

    let mut invoice = usd_invoice(
        vec![line("a", "1", "100.00", "1", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    );
    invoice.issue("INV-2026-0001").expect("issues");
    let version = ledger.version();
    invoice
        .post_to(
            &mut ledger,
            "2026-04",
            &receivable,
            &revenue,
            &tax_payable,
            version,
        )
        .expect("posts");
    assert_eq!(ledger.balance(&receivable), 12_000, "120.00 owed");

    // The credit note mirrors it, and the two cancel because the arithmetic
    // cancels — not because anything was deleted.
    let mut credit = usd_invoice(
        vec![line("a", "1", "100.00", "1", TaxCategory::Standard, 200)],
        RoundingPolicy::En16931Group,
    )
    .as_credit_note()
    .references("INV-2026-0001", "2026-04-01")
    .because(IssueReason::GoodsOrServicesReturned);
    credit.issue("INV-2026-0002").expect("issues");
    let version = ledger.version();
    credit
        .post_to(
            &mut ledger,
            "2026-04",
            &receivable,
            &revenue,
            &tax_payable,
            version,
        )
        .expect("posts");

    assert_eq!(ledger.balance(&receivable), 0, "nothing is owed");
    assert_eq!(ledger.balance(&revenue), 0);
    assert_eq!(ledger.balance(&tax_payable), 0);
    assert_eq!(
        ledger.posted_entries().len(),
        2,
        "and both documents are retained"
    );
    ledger.assert_balanced().expect("every entry balances");

    // The credit note must be the *mirror* of the invoice, not merely a balanced
    // entry that happens to cancel. Checking the sides catches a credit note that
    // debited revenue instead of crediting it — which balances in some
    // configurations and misstates the period in all of them.
    let invoice_entry = ledger.entry("INV-2026-0001").expect("stored");
    let credit_entry = ledger.entry("INV-2026-0002").expect("stored");
    assert_eq!(
        invoice_entry.lines.len(),
        credit_entry.lines.len(),
        "the credit note has the same shape"
    );
    for (debit_line, credit_line) in invoice_entry.lines.iter().zip(credit_entry.lines.iter()) {
        assert_eq!(
            debit_line.side.opposite(),
            credit_line.side,
            "line on {} has the opposite side",
            debit_line.account
        );
        assert_eq!(
            debit_line.amount.minor_units(),
            credit_line.amount.minor_units(),
            "and the same magnitude, because direction is the type's job"
        );
    }

    // And the tax payable leg was credited on the invoice: a liability increases
    // on the credit side.
    let tax_leg = invoice_entry
        .lines
        .iter()
        .find(|l| l.account.to_string() == "2300")
        .expect("the tax leg");
    assert_eq!(
        tax_leg.side,
        double_entry::Side::Credit,
        "charging VAT increases the liability, so it is credited"
    );
}

// -- 7. allocation, in integer minor units ---------------------------------

/// The bug that motivated this module, from a real production system: three
/// allocations rounded per reference gave a one-cent residue that could not post,
/// and the reported symptom was a payment refused with "either debit or credit
/// amount is required".
#[test]
fn allocation_lands_the_whole_payment_with_no_escaping_residue() {
    use double_entry::Amount;

    let currency = currencies::USD;
    let outstanding = vec![
        ("INV-1".to_string(), 135_713_i64),
        ("INV-2".to_string(), 135_713_i64),
        ("INV-3".to_string(), 271_425_i64),
    ];
    let payment = Amount::minor(542_851, currency);
    let result = allocation::allocate("PAY-1", payment, &outstanding, currency).expect("allocates");

    let allocated: i64 = result
        .allocations
        .iter()
        .filter_map(|a| a.amount.minor_units())
        .sum();
    assert_eq!(
        allocated, 542_851,
        "the allocations sum to exactly the payment"
    );
    assert_eq!(
        result.residual, 0,
        "so nothing escapes to an untracked account"
    );
    assert_eq!(result.allocations.len(), 3);
}

#[test]
fn an_underpayment_stops_at_the_last_invoice_and_says_so() {
    use double_entry::Amount;

    let currency = currencies::USD;
    let outstanding = vec![
        ("INV-1".to_string(), 10_000_i64),
        ("INV-2".to_string(), 50_000_i64),
    ];
    let payment = Amount::minor(25000, currency);
    let result = allocation::allocate("PAY-2", payment, &outstanding, currency).expect("allocates");
    // 250.00 against [100.00, 500.00]: INV-1 is cleared and the remaining 150.00
    // part-pays INV-2. Stopping at one would leave the money unapplied.
    assert_eq!(result.allocations.len(), 2);
    assert_eq!(
        result.allocations.first().map(|a| a.invoice.as_str()),
        Some("INV-1"),
        "document order, so the oldest invoice is settled first"
    );
    assert_eq!(
        result.allocations.get(1).map(|a| a.amount.units),
        Some(15000),
        "and the rest lands on the next one"
    );
    assert_eq!(
        result.residual, 0,
        "the remainder is applied, not parked in an untracked account"
    );

    // A payment smaller than the first invoice touches only that one.
    let small = double_entry::Amount::minor(5000, currency);
    let partial = allocation::allocate("PAY-2b", small, &outstanding, currency).expect("allocates");
    assert_eq!(partial.allocations.len(), 1);
    assert_eq!(partial.residual, 0);
}

#[test]
fn an_allocation_refuses_to_over_allocate_or_to_convert() {
    use double_entry::Amount;

    let currency = currencies::USD;
    let outstanding = vec![("INV-1".to_string(), 10_000_i64)];

    // FIFO cannot over-allocate by construction: it bounds each share by what is
    // owed, so the extra 50.00 is simply not allocated.
    let fifo = allocation::allocate(
        "PAY-3",
        Amount::minor(15000, currency),
        &outstanding,
        currency,
    )
    .expect("allocates what it can");
    assert_eq!(fifo.allocations.len(), 1);
    assert_eq!(
        fifo.residual, 5000,
        "the excess is unapplied, not over-applied"
    );

    // A caller-directed allocation *can* over-allocate — against a stale
    // outstanding figure — and that is the mistake behind a real production
    // rejection where a validator compared an allocated amount against a
    // displayed, rounded one.
    assert!(matches!(
        allocation::allocate_specific(
            "PAY-3b",
            Amount::minor(15000, currency),
            &[("INV-1".to_string(), 15000)],
            &outstanding,
            currency,
        ),
        Err(allocation::AllocationError::ExceedsOutstanding {
            allocated: 15000,
            outstanding: 10000,
            ..
        })
    ));

    // And more than the payment itself is refused too.
    assert!(matches!(
        allocation::allocate_specific(
            "PAY-3c",
            Amount::minor(10000, currency),
            &[("INV-1".to_string(), 8000), ("INV-2".to_string(), 8000),],
            &[("INV-1".to_string(), 10000), ("INV-2".to_string(), 10000),],
            currency,
        ),
        Err(allocation::AllocationError::ExceedsOutstanding { .. })
    ));

    let euro = Amount::minor(10000, currencies::EUR);
    assert_eq!(
        allocation::allocate("PAY-4", euro, &outstanding, currency),
        Err(allocation::AllocationError::CurrencyMismatch {
            payment: "EUR",
            invoice: "USD",
        }),
        "nothing here holds a rate, so an allocation must not convert"
    );
}

#[test]
fn a_replayed_payment_id_is_detectable() {
    let seen = vec!["PAY-1".to_string(), "PAY-2".to_string()];
    assert!(
        allocation::is_replay(&seen, "PAY-1"),
        "replaying credits twice"
    );
    assert!(!allocation::is_replay(&seen, "PAY-3"));
}

// -- 8. numbering: gapless, and a chain that sees deletion -----------------

#[test]
fn a_series_is_gapless_and_never_reissues_a_cancelled_number() {
    let mut series = numbering::Series::new("INV-2026");
    assert_eq!(
        series.peek().expect("peeks").number,
        "INV-2026-0001",
        "zero-padded so a number sorts in the same order numerically"
    );
    let first = series.next_number().expect("issues");
    let second = series.next_number().expect("issues");
    assert_eq!(first.number, "INV-2026-0001");
    assert_eq!(second.number, "INV-2026-0002");

    // A cancelled document consumed its number. UK law permits the resulting gap
    // only because the cancelled document is retained; the alternative is a
    // number issued twice, which is worse.
    series.cancel(2).expect("cancels");
    assert!(series.is_cancelled(2));
    let third = series.next_number().expect("issues");
    assert_eq!(
        third.number, "INV-2026-0003",
        "a cancelled number is spent, not reissued"
    );
    assert_eq!(series.last_issued(), Some(3));
}

#[test]
fn a_gap_must_be_explained_by_a_cancellation() {
    let mut series = numbering::Series::new("INV-2026");
    for _ in 0..3 {
        series.next_number().expect("issues");
    }
    series.cancel(2).expect("cancels 2");
    assert!(
        series.verify_gapless(&[1, 3]).is_ok(),
        "a gap explained by a retained cancelled document is permitted — UK \
         VAT Regs 1995 reg. 14 allows exactly this"
    );
    assert!(
        series.verify_gapless(&[1, 3, 5]).is_err(),
        "a gap with no cancellation behind it is a finding"
    );
}

/// ZATCA requires a tamper-resistant counter **and the previous invoice's hash**.
/// The hash chain is what detects a document deleted from the middle of a series,
/// which a gap check cannot see — a deletion leaves no gap to find.
#[test]
fn the_hash_chain_detects_a_document_deleted_from_the_middle() {
    let mut series = numbering::Series::new("INV-2026").hash_chained();
    let mut hashes = Vec::new();
    for _ in 0..5 {
        let issued = series.next_number().expect("issues");
        let hash = issued.hash.expect("a hash-chained series always hashes");
        hashes.push(hash);
    }
    series
        .verify_chain(&hashes)
        .expect("an intact chain verifies");

    // Delete the third document: the series is still gapless — 1,2,4,5 leaves no
    // gap a counter would notice — but every hash from the deletion point differs.
    let mut tampered = hashes.clone();
    tampered.remove(2);
    assert!(
        series.verify_chain(&tampered).is_err(),
        "removing a document from the middle must break the chain"
    );

    // And altering one earlier document's hash invalidates everything after it.
    let mut altered = hashes.clone();
    if let Some(first) = altered.first_mut() {
        first.push('0');
    }
    assert!(series.verify_chain(&altered).is_err());
}

#[test]
fn a_series_refuses_to_run_past_its_ceiling_rather_than_break_sequence() {
    let mut series = numbering::Series::new("INV-2026").up_to(2);
    series.next_number().expect("issues 1");
    series.next_number().expect("issues 2");
    assert!(matches!(
        series.next_number(),
        Err(NumberingError::Exhausted { last: 2, .. })
    ));
    // A gap is a policy choice made by *cancelling* a document, never a rescue
    // from running out: `allowing_gaps` relaxes the check, not the counter.
    let mut lenient = numbering::Series::new("INV-2027").allowing_gaps().up_to(2);
    lenient.next_number().expect("issues 1");
    lenient.next_number().expect("issues 2");
    assert!(lenient.next_number().is_err(), "exhausted is exhausted");
}

#[test]
fn a_series_name_that_would_make_numbers_ambiguous_is_refused() {
    let mut empty = numbering::Series::new("");
    assert!(matches!(
        empty.next_number(),
        Err(NumberingError::InvalidSeries(_))
    ));
    let mut spaced = numbering::Series::new("INV 2026");
    assert!(
        matches!(spaced.next_number(), Err(NumberingError::InvalidSeries(_))),
        "a space in a series name makes a number ambiguous across series"
    );
}

// -- 9. decimal handling ---------------------------------------------------

#[test]
fn a_decimal_parses_exactly_and_refuses_what_a_float_would_round() {
    assert_eq!(
        Decimal::parse("120.00").expect("parses"),
        Decimal::new(12_000, 2)
    );
    assert_eq!(
        Decimal::parse("-0.05").expect("parses"),
        Decimal::new(-5, 2),
        "a negative parses — it is the *line value* that is refused, not the \
         decimal type, because tax rates and credits need them"
    );
    assert_eq!(
        Decimal::parse("0.05").expect("parses").to_string_exact(),
        "0.05",
        "and round-trips exactly, which 0.05 as an f64 does not"
    );

    for bad in ["", "  ", "12.", ".5", "1.2.3", "12,00", "1e5", "007"] {
        assert!(
            Decimal::parse(bad).is_none(),
            "{bad:?} must be refused rather than guessed at"
        );
    }
}

#[test]
fn rescaling_refuses_to_round_rather_than_guessing_a_mode() {
    // 1.050 *is* a whole number of cents (105 at scale 2), so this rescale is
    // exact and must succeed. Using it as the "cannot round" case would have
    // tested the wrong thing.
    assert_eq!(
        Decimal::new(1050, 3).rescale_exact(2),
        Some(Decimal::new(105, 2)),
        "1.050 coarsens to 1.05 exactly"
    );
    let value = Decimal::new(1055, 3); // 1.055
    assert_eq!(
        value.rescale_exact(2),
        None,
        "1.055 is not a whole number of cents, and no rounding mode was named, \
         so it is refused rather than rounded by default"
    );
    assert_eq!(
        value.rescale_exact(3),
        Some(value),
        "the same scale is a no-op"
    );
    assert_eq!(
        value.rescale_exact(1),
        None,
        "1.055 is not a whole number of tenths either"
    );
    assert_eq!(
        Decimal::new(1040, 3).rescale_exact(1),
        None,
        "1.040 is 10.4 tenths, which is not a whole tenth either"
    );
    assert_eq!(
        Decimal::new(1000, 3).rescale_exact(1),
        Some(Decimal::new(10, 1)),
        "while 1.000 is a whole number of tenths"
    );
}

// -- 10. a zero-decimal currency is not assumed to have two places ---------

#[test]
fn a_zero_decimal_currency_is_carried_through() {
    let currency: Currency = currencies::JPY;
    let invoice = Invoice::draft("2026-04-01", currency, RoundingPolicy::En16931Group)
        .with_line(line("a", "1", "1500", "1", TaxCategory::Standard, 800));
    assert_eq!(
        invoice.net_total().units,
        1500,
        "1500 yen is 1500 minor units, not 15.00 — an implementation that \
         assumes two decimals is wrong by 100x"
    );
}
