//! Tamper-evident, hash-chained log of privileged actions.
//!
//! Each record commits to the digest of the record before it. Removing,
//! reordering or editing a record changes every digest after it, so anyone
//! who replays the chain from genesis can detect the change with
//! [`verify_chain`].

use sha2::{Digest, Sha256};

use crate::Address;

/// A 32-byte SHA-256 digest.
pub type Digest32 = [u8; 32];

/// The `prev` value of the first record in a chain.
pub const GENESIS: Digest32 = [0u8; 32];

/// One privileged action, chained to the record before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    /// Position in the chain, starting at zero.
    pub seq: u64,
    /// Account that performed the action.
    pub caller: Address,
    /// Action tag, for example [`crate::ACTION_CREATE_GRANT`].
    pub action: u32,
    /// SHA-256 of the action's arguments. The arguments are not stored.
    pub payload_digest: Digest32,
    /// Ledger the action was recorded in.
    pub ledger: u64,
    /// Digest of the previous record, or [`GENESIS`] for the first one.
    pub prev: Digest32,
    /// Digest of this record, covering every field above.
    pub digest: Digest32,
}

/// Append-only log of [`AuditRecord`]s.
#[derive(Debug, Clone, Default)]
pub struct AuditLog {
    records: Vec<AuditRecord>,
}

impl AuditLog {
    /// Creates an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Digest of the newest record, or [`GENESIS`] for an empty log.
    pub fn head(&self) -> Digest32 {
        self.records.last().map_or(GENESIS, |record| record.digest)
    }

    /// The record at `seq`, if any.
    pub fn get(&self, seq: u64) -> Option<&AuditRecord> {
        self.records.get(usize::try_from(seq).ok()?)
    }

    /// All records, oldest first.
    pub fn records(&self) -> &[AuditRecord] {
        &self.records
    }

    /// Appends one record for `action` performed by `caller` on `ledger`.
    /// `payload` is the encoded arguments; only its digest is stored.
    /// Returns the new record's sequence number.
    pub fn append(&mut self, caller: Address, action: u32, payload: &[u8], ledger: u64) -> u64 {
        let seq = self.records.len() as u64;
        let prev = self.head();
        let payload_digest: Digest32 = Sha256::digest(payload).into();
        let digest = chain_digest(seq, caller, action, &payload_digest, ledger, &prev);
        self.records.push(AuditRecord {
            seq,
            caller,
            action,
            payload_digest,
            ledger,
            prev,
            digest,
        });
        seq
    }
}

/// Replays `records` from genesis. Returns the index of the first record that
/// breaks the chain: a wrong sequence number, a broken link to the previous
/// record, or a digest that does not match its fields.
pub fn verify_chain(records: &[AuditRecord]) -> Result<(), usize> {
    let mut prev = GENESIS;
    for (index, record) in records.iter().enumerate() {
        let expected = chain_digest(
            record.seq,
            record.caller,
            record.action,
            &record.payload_digest,
            record.ledger,
            &record.prev,
        );
        if record.seq != index as u64 || record.prev != prev || record.digest != expected {
            return Err(index);
        }
        prev = record.digest;
    }
    Ok(())
}

fn chain_digest(
    seq: u64,
    caller: Address,
    action: u32,
    payload_digest: &Digest32,
    ledger: u64,
    prev: &Digest32,
) -> Digest32 {
    let mut hasher = Sha256::new();
    hasher.update(seq.to_be_bytes());
    hasher.update(caller.to_be_bytes());
    hasher.update(action.to_be_bytes());
    hasher.update(payload_digest);
    hasher.update(ledger.to_be_bytes());
    hasher.update(prev);
    hasher.finalize().into()
}
