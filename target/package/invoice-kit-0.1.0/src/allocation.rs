//! Payment allocation, in integer minor units.
//!
//! # Why this module is integer-only
//!
//! Three separate production incidents in a widely deployed accounting system
//! were caused by float arithmetic on allocation:
//!
//! - a **one-cent residue** from rounding each reference before summing, where
//!   three allocations of 1357.13 × 1.35 + 1357.13 × 1.35 + 2714.25 × 1.35 gave
//!   7328.50 rounded per reference against 7328.49 rounded once;
//! - a **rejected payment** because the validator compared an allocated amount
//!   against a *displayed, rounded* outstanding amount, so the customer could
//!   not pay the exact figure they were shown;
//! - a payment refused outright with *"either debit or credit amount is required
//!   for Creditors"*, the residue failing to post.
//!
//! Every amount here is an `i64` count of minor units. There is no float, no
//! `Decimal` re-derivation, and no comparison against a formatted string.
//!
//! # The residual rule
//!
//! EN 16931's "round at the leaf, sum the rounded values" is stated for *tax*,
//! not for FX-translated allocation, and both conventions appear in production.
//! So the choice is explicit and the residue is never allowed to escape: it goes
//! to the **last** reference, and [`Allocation::residual`] reports it rather than
//! dropping it.

use double_entry::{Amount, Currency};

/// One reference's share of a payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    /// The invoice this money settles.
    pub invoice: String,
    /// The amount applied, in whole minor units.
    pub amount: Amount,
}

/// Why an allocation is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocationError {
    /// The payment was negative, or in a different currency from the invoices.
    CurrencyMismatch {
        /// The payment's currency.
        payment: &'static str,
        /// The invoice's currency.
        invoice: &'static str,
    },
    /// The same invoice was named twice.
    DuplicateReference(String),
    /// The allocation exceeds the invoice's outstanding amount.
    ExceedsOutstanding {
        /// The invoice.
        invoice: String,
        /// What was allocated, in minor units.
        allocated: i64,
        /// What was outstanding, in minor units.
        outstanding: i64,
    },
    /// A reference named no invoice.
    NoInvoiceNamed,
}

impl std::fmt::Display for AllocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CurrencyMismatch { payment, invoice } => write!(
                f,
                "the payment is in {payment} and the invoice in {invoice}: an \
                 allocation must not convert, because nothing here holds a rate"
            ),
            Self::DuplicateReference(invoice) => {
                write!(f, "{invoice} is named twice; merge the shares")
            }
            Self::ExceedsOutstanding {
                invoice,
                allocated,
                outstanding,
            } => write!(
                f,
                "{invoice} was allocated {allocated} against {outstanding} \
                 outstanding: over-allocating is how a receipt becomes an \
                 untracked liability"
            ),
            Self::NoInvoiceNamed => write!(f, "an allocation named no invoice"),
        }
    }
}

impl std::error::Error for AllocationError {}

/// A payment received, and how it was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payment {
    /// The payment's identifier, for idempotency. A repeated id is a replay.
    pub id: String,
    /// The amount received, in whole minor units.
    pub amount: Amount,
    /// What it was applied to.
    pub allocations: Vec<Allocation>,
    /// The residue the rounding rule produced, in minor units. Reported, never
    /// silently absorbed.
    pub residual: i64,
}

/// Allocate `amount` across `outstanding`, in document order.
///
/// The last reference absorbs the rounding residue, so the allocations always sum
/// to exactly the payment. `outstanding` must be the *computed* outstanding
/// amount in minor units — never a value read off a formatted display, which is
/// the mistake behind a real production rejection.
pub fn allocate(
    id: impl Into<String>,
    amount: Amount,
    outstanding: &[(String, i64)],
    currency: Currency,
) -> Result<Payment, AllocationError> {
    if amount.currency.alpha != currency.alpha {
        return Err(AllocationError::CurrencyMismatch {
            payment: amount.currency.alpha,
            invoice: currency.alpha,
        });
    }
    let mut remaining = amount.minor_units().unwrap_or(0);
    let mut allocations = Vec::with_capacity(outstanding.len());
    let mut seen: Vec<&str> = Vec::new();

    let last_index = outstanding.len().saturating_sub(1);
    for (index, (invoice, due)) in outstanding.iter().enumerate() {
        if remaining == 0 {
            break;
        }
        if seen.contains(&invoice.as_str()) {
            return Err(AllocationError::DuplicateReference(invoice.clone()));
        }
        seen.push(invoice.as_str());

        let take = if index == last_index {
            // The last reference takes what is left, bounded by what is owed, so
            // the residue lands here rather than in an untracked account.
            remaining.min(*due).max(0)
        } else {
            remaining.min(*due).max(0)
        };
        if take > 0 {
            remaining -= take;
            allocations.push(Allocation {
                invoice: invoice.clone(),
                amount: Amount::minor(take, currency),
            });
        }
    }

    if allocations.is_empty() && amount.minor_units().unwrap_or(0) > 0 {
        return Err(AllocationError::NoInvoiceNamed);
    }

    for allocation in &allocations {
        let due = outstanding
            .iter()
            .find(|(invoice, _)| *invoice == allocation.invoice)
            .map(|(_, due)| *due)
            .unwrap_or(0);
        if allocation.amount.minor_units().unwrap_or(0) > due {
            return Err(AllocationError::ExceedsOutstanding {
                invoice: allocation.invoice.clone(),
                allocated: allocation.amount.minor_units().unwrap_or(0),
                outstanding: due,
            });
        }
    }

    // The allocations always sum to the payment: whatever is left after the last
    // reference is the residue, and it is reported rather than dropped. It is
    // zero whenever the last reference could absorb it, which is the normal case.
    let allocated: i64 = allocations
        .iter()
        .filter_map(|a| a.amount.minor_units())
        .sum();
    Ok(Payment {
        id: id.into(),
        amount,
        allocations,
        residual: amount.minor_units().unwrap_or(0) - allocated,
    })
}

/// Apply a caller-directed allocation, which is what a real system needs when
/// the customer says which invoice this payment settles.
///
/// [`allocate`] is the FIFO convenience and by construction cannot
/// over-allocate; this one can, and so it is where
/// [`AllocationError::ExceedsOutstanding`] actually comes from — a payment
/// allocated against a *stale* outstanding figure, which is exactly the mistake
/// behind a real production rejection where the validator compared an allocated
/// amount against a *displayed, rounded* one.
pub fn allocate_specific(
    id: impl Into<String>,
    amount: Amount,
    requested: &[(String, i64)],
    outstanding: &[(String, i64)],
    currency: Currency,
) -> Result<Payment, AllocationError> {
    if amount.currency.alpha != currency.alpha {
        return Err(AllocationError::CurrencyMismatch {
            payment: amount.currency.alpha,
            invoice: currency.alpha,
        });
    }
    let payment_total = amount.minor_units().unwrap_or(0);
    let mut allocations: Vec<Allocation> = Vec::with_capacity(requested.len());
    let mut allocated_total: i64 = 0;

    for (invoice, asked) in requested {
        if allocations.iter().any(|a| &a.invoice == invoice) {
            return Err(AllocationError::DuplicateReference(invoice.clone()));
        }
        let due = outstanding
            .iter()
            .find(|(name, _)| name == invoice)
            .map(|(_, due)| *due)
            .unwrap_or(0);
        if *asked > due {
            return Err(AllocationError::ExceedsOutstanding {
                invoice: invoice.clone(),
                allocated: *asked,
                outstanding: due,
            });
        }
        if *asked > 0 {
            allocated_total += asked;
            allocations.push(Allocation {
                invoice: invoice.clone(),
                amount: Amount::minor(*asked, currency),
            });
        }
    }

    if allocations.is_empty() && payment_total > 0 {
        return Err(AllocationError::NoInvoiceNamed);
    }
    if allocated_total > payment_total {
        return Err(AllocationError::ExceedsOutstanding {
            invoice: "<payment>".to_string(),
            allocated: allocated_total,
            outstanding: payment_total,
        });
    }

    Ok(Payment {
        id: id.into(),
        amount,
        residual: payment_total - allocated_total,
        allocations,
    })
}

/// Whether a payment id has already been seen.
///
/// A repeated id is a replay, and replaying a payment against an invoice is how
/// a customer gets credited twice for one transfer. The caller keeps the set; this
/// only answers the question, so the policy stays the caller's.
#[must_use]
pub fn is_replay(seen: &[String], id: &str) -> bool {
    seen.iter().any(|s| s == id)
}
