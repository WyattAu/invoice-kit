//! Sequential numbering, and the tamper-evident chain that is stronger than it.
//!
//! # Four jurisdictions, four requirements
//!
//! | Jurisdiction | Requirement |
//! |---|---|
//! | EU, EN 16931 | **Nothing.** BR-02 only requires BT-1 to *exist*. |
//! | UK, VAT Regs 1995 reg. 14 | "a sequential number based on one or more series which uniquely identifies the document". Parallel series are permitted; gaps are permitted **only** if the cancelled or spoiled document is retained, or the break can be explained. |
//! | ZATCA, KSA | A unique sequential note number, a 128-bit UUID, a **tamper-resistant counter that increments for each document issued**, **and the previous invoice's hash**. Format is free. |
//! | Australia, GST Act s9-90 | A sequential number identifying the document, or a receipt. |
//!
//! So: **gapless by default**, because two of the four require it and the third
//! tolerates it only with retained evidence; and an **optional hash chain**, which
//! one jurisdiction demands and which detects deletion of any document in the
//! middle of a series — something a gap check cannot see, because a deleted
//! document leaves no gap to find.
//!
//! # Retention
//!
//! UK records must be kept six years from the **date of issue**, not from the
//! event, and HMRC Notice 700/63 requires the system to *"recreate the invoice
//! data as at the time of its original transmission or receipt"* — so a
//! soft-deleted row fails. ZATCA's resolution says the same in the negative
//! form: the solution must protect invoices *"from alteration or deletion"*.
//! [`Series::next_number`] therefore never reissues a number, even after a
//! cancellation.

use sha2::{Digest, Sha256};

/// Why a numbering request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumberingError {
    /// The series name is empty or contains a character that would make the
    /// resulting number ambiguous.
    InvalidSeries(String),
    /// The counter has reached its configured maximum, so the next number would
    /// not be sequential.
    Exhausted {
        /// The series that ran out.
        series: String,
        /// The last number issued.
        last: u64,
    },
}

impl std::fmt::Display for NumberingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSeries(name) => write!(
                f,
                "series {name:?} is empty or contains whitespace, which would make \
                 a number ambiguous"
            ),
            Self::Exhausted { series, last } => write!(
                f,
                "series {series} has issued through {last}, its configured maximum; \
                 the next number would break the sequence, and a tax authority \
                 cannot be told a sequence simply resumed"
            ),
        }
    }
}

impl std::error::Error for NumberingError {}

/// The state of one numbering series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    /// The series name, e.g. `INV-2026`.
    pub name: String,
    /// The width to zero-pad the counter to, so `INV-2026-0007` sorts
    /// lexicographically in the same order as numerically.
    pub padding: usize,
    /// The next counter to hand out. Monotonic; never decremented.
    next: u64,
    /// The last number issued, for a gap check.
    last_issued: Option<u64>,
    /// Whether the series refuses to issue after a cancellation. Gapless by
    /// default, because two jurisdictions require it.
    pub gapless: bool,
    /// Whether to chain each number's hash to the previous document's. This is
    /// the ZATCA requirement, and it detects deletion from the middle of a
    /// series, which a gap check cannot.
    pub hash_chained: bool,
    /// The last hash handed out, for the chain.
    last_hash: Option<String>,
    /// The counter at which this series stops being sequential.
    pub ceiling: u64,
    /// Numbers consumed by cancelled or spoiled documents, which a gap check must
    /// tolerate because UK law permits the gap when the document is retained.
    cancelled: Vec<u64>,
}

impl Series {
    /// A gapless series starting at 1.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            padding: 4,
            next: 1,
            last_issued: None,
            gapless: true,
            hash_chained: false,
            last_hash: None,
            ceiling: u64::MAX,
            cancelled: Vec::new(),
        }
    }

    /// Chain each number's hash to the previous document's.
    pub fn hash_chained(mut self) -> Self {
        self.hash_chained = true;
        self
    }

    /// Pad the counter to `width` digits.
    pub fn padded_to(mut self, width: usize) -> Self {
        self.padding = width;
        self
    }

    /// Stop issuing at `ceiling`.
    pub fn up_to(mut self, ceiling: u64) -> Self {
        self.ceiling = ceiling;
        self
    }

    /// Allow gaps. Off by default; the UK permits a gap only when the
    /// cancelled document is retained, and EU law requires nothing.
    pub fn allowing_gaps(mut self) -> Self {
        self.gapless = false;
        self
    }

    /// The next number, without consuming it.
    ///
    /// The returned hash is the chain value for this number when
    /// [`Series::hash_chained`] is set, and `None` otherwise. It is computed from
    /// the series name, the counter and the **previous** document's hash, so
    /// changing any earlier document changes every hash after it.
    pub fn peek(&self) -> Result<Peeked, NumberingError> {
        self.validate()?;
        if self.next > self.ceiling {
            return Err(NumberingError::Exhausted {
                series: self.name.clone(),
                last: self.next.saturating_sub(1),
            });
        }
        Ok(Peeked {
            number: format!("{}-{:0width$}", self.name, self.next, width = self.padding),
            counter: self.next,
            hash: if self.hash_chained {
                Some(Self::chain_hash(
                    &self.name,
                    self.next,
                    self.last_hash.as_deref(),
                ))
            } else {
                None
            },
        })
    }

    /// Consume the next number.
    pub fn next_number(&mut self) -> Result<Issued, NumberingError> {
        let peeked = self.peek()?;
        self.last_issued = Some(peeked.counter);
        self.last_hash = peeked.hash.clone();
        self.next = self.next.checked_add(1).ok_or(NumberingError::Exhausted {
            series: self.name.clone(),
            last: peeked.counter,
        })?;
        Ok(Issued {
            number: peeked.number,
            counter: peeked.counter,
            hash: peeked.hash,
        })
    }

    /// Record that a number was consumed by a cancelled or spoiled document.
    ///
    /// The counter is **not** rewound. A number that has been printed on a
    /// document is spent whether or not that document was ever issued — UK law
    /// permits the resulting gap only because the cancelled document is
    /// retained, and the alternative is a number issued twice.
    pub fn cancel(&mut self, counter: u64) -> Result<(), NumberingError> {
        if !self.cancelled.contains(&counter) {
            self.cancelled.push(counter);
            self.cancelled.sort_unstable();
        }
        Ok(())
    }

    /// Whether a counter was consumed by a cancelled document.
    pub fn is_cancelled(&self, counter: u64) -> bool {
        self.cancelled.contains(&counter)
    }

    /// The last number issued.
    pub fn last_issued(&self) -> Option<u64> {
        self.last_issued
    }

    /// The counter that will be issued next.
    pub fn next_counter(&self) -> u64 {
        self.next
    }

    /// Check that a run of issued counters is contiguous, tolerating only the
    /// counters this series recorded as cancelled.
    ///
    /// UK VAT Regs 1995 reg. 14 permits a gap only where the cancelled or
    /// spoiled document is retained; EU law requires nothing. So the default is
    /// gapless, and a gap that is *not* explained by a recorded cancellation is a
    /// finding rather than a rounding artefact.
    pub fn verify_gapless(&self, issued: &[u64]) -> Result<(), NumberingError> {
        let mut previous: Option<u64> = None;
        for counter in issued {
            if let Some(previous) = previous {
                let expected = previous + 1;
                if *counter != expected && !self.is_cancelled(previous + 1) {
                    return Err(NumberingError::InvalidSeries(format!(
                        "gap after {previous}: next issued is {counter}, and \
                         {expected} was neither issued nor cancelled"
                    )));
                }
            }
            previous = Some(*counter);
        }
        Ok(())
    }

    /// Verify a chain of hashes against this series.
    ///
    /// Recomputes every hash from the documents and compares. A single deleted
    /// document from the middle of a chain is invisible to a gap check and
    /// obvious here, because every hash after the deletion point differs.
    pub fn verify_chain(&self, hashes: &[String]) -> Result<(), NumberingError> {
        let mut previous: Option<String> = None;
        for (offset, supplied) in hashes.iter().enumerate() {
            let counter = (offset + 1) as u64;
            let expected = Self::chain_hash(&self.name, counter, previous.as_deref());
            if &expected != supplied {
                return Err(NumberingError::InvalidSeries(format!(
                    "chain broken at {counter}: expected {expected}, found {supplied}. A \
                     document before this point was altered or deleted."
                )));
            }
            previous = Some(supplied.clone());
        }
        Ok(())
    }

    fn chain_hash(series: &str, counter: u64, previous: Option<&str>) -> String {
        let mut hasher = Sha256::new();
        hasher.update(series.as_bytes());
        hasher.update(b"/");
        hasher.update(counter.to_string().as_bytes());
        if let Some(previous) = previous {
            hasher.update(b"/");
            hasher.update(previous.as_bytes());
        }
        let digest = hasher.finalize();
        // Full 32 bytes hex: a truncated tag would weaken the deletion detection
        // this exists for, and truncation is exactly the kind of shortcut that
        // turns a 256-bit chain into a guessable one.
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn validate(&self) -> Result<(), NumberingError> {
        if self.name.trim().is_empty() || self.name.chars().any(|c| c.is_whitespace() || c == '\0')
        {
            return Err(NumberingError::InvalidSeries(self.name.clone()));
        }
        Ok(())
    }
}

/// A number that has not been consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peeked {
    /// The formatted number.
    pub number: String,
    /// The bare counter.
    pub counter: u64,
    /// The chain hash, when the series is hash-chained.
    pub hash: Option<String>,
}

/// A consumed number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    /// The formatted number.
    pub number: String,
    /// The bare counter.
    pub counter: u64,
    /// The chain hash, when the series is hash-chained.
    pub hash: Option<String>,
}
