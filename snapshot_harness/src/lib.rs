//! Snapshot harness — loads a frozen ledger fixture and exposes helpers
//! for running integration tests against it.
//!
//! The fixture lives at `snapshot_harness/fixtures/testnet-ledger.json`.
//! See `snapshot_harness/fixtures/README.md` for how to regenerate it.

/// Raw JSON bytes of the testnet ledger fixture, embedded at compile time.
pub const TESTNET_LEDGER_JSON: &str =
    include_str!("../fixtures/testnet-ledger.json");

/// The ledger sequence recorded in the checked-in fixture.
/// Tests that need a deterministic timestamp can use this as a baseline.
pub fn fixture_ledger_sequence() -> u32 {
    // Parse just enough of the JSON to extract the sequence number without
    // pulling in a full JSON library as a non-dev dependency.
    for line in TESTNET_LEDGER_JSON.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"ledger_sequence\":") {
            let value = rest.trim().trim_end_matches(',');
            if let Ok(seq) = value.trim().parse::<u32>() {
                return seq;
            }
        }
    }
    panic!("ledger_sequence not found in testnet-ledger.json");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_json_is_valid_utf8() {
        assert!(!TESTNET_LEDGER_JSON.is_empty());
    }

    #[test]
    fn fixture_ledger_sequence_is_nonzero() {
        assert!(fixture_ledger_sequence() > 0);
    }
}
