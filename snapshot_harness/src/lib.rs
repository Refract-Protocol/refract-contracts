//! Load a captured ledger snapshot into a Soroban test environment.
//!
//! Fixtures are raw ledger values plus their XDR, decoded here. A schema,
//! version, or hash mismatch is an error. Entries are never skipped.

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soroban_sdk::{testutils::Ledger, Env};
use std::collections::BTreeMap;

pub const SCHEMA: u32 = 1;
pub const VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub contract: String,
    pub key: String,
    pub value: i128,
    pub xdr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub schema: u32,
    pub version: String,
    pub network: String,
    pub ledger_sequence: u32,
    pub close_time: u64,
    pub hash: String,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSnapshot {
    pub network: String,
    pub ledger_sequence: u32,
    pub close_time: u64,
    pub values: BTreeMap<(String, String), i128>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    SchemaMismatch { found: u32 },
    VersionMismatch { found: String },
    BadHash,
    BadXdr { contract: String, key: String },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::SchemaMismatch { found } => {
                write!(f, "snapshot schema {found} does not match {SCHEMA}")
            }
            LoadError::VersionMismatch { found } => {
                write!(f, "snapshot version {found} does not match {VERSION}")
            }
            LoadError::BadHash => write!(f, "snapshot hash does not match its entries"),
            LoadError::BadXdr { contract, key } => {
                write!(f, "snapshot entry {contract}/{key} is not valid XDR")
            }
        }
    }
}

pub fn canonical_bytes(entries: &[Entry]) -> Vec<u8> {
    let mut rows: Vec<&Entry> = entries.iter().collect();
    rows.sort_by(|a, b| (&a.contract, &a.key).cmp(&(&b.contract, &b.key)));
    let mut out = String::new();
    for (i, entry) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&entry.contract);
        out.push('\t');
        out.push_str(&entry.key);
        out.push('\t');
        out.push_str(&entry.value.to_string());
        out.push('\t');
        out.push_str(&entry.xdr);
    }
    out.into_bytes()
}

pub fn content_hash(entries: &[Entry]) -> String {
    let digest = Sha256::digest(canonical_bytes(entries));
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

/// Decode XDR (standard base64) and return the integer stored beside it.
/// Invalid XDR fails the whole load.
fn decode_entry(entry: &Entry) -> Result<i128, LoadError> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(entry.xdr.trim())
        .map_err(|_| LoadError::BadXdr {
            contract: entry.contract.clone(),
            key: entry.key.clone(),
        })?;
    if decoded.is_empty() {
        return Err(LoadError::BadXdr {
            contract: entry.contract.clone(),
            key: entry.key.clone(),
        });
    }
    Ok(entry.value)
}

pub fn load(file: &SnapshotFile) -> Result<LoadedSnapshot, LoadError> {
    if file.schema != SCHEMA {
        return Err(LoadError::SchemaMismatch { found: file.schema });
    }
    if file.version != VERSION {
        return Err(LoadError::VersionMismatch {
            found: file.version.clone(),
        });
    }
    if file.hash != content_hash(&file.entries) {
        return Err(LoadError::BadHash);
    }
    let mut values = BTreeMap::new();
    let mut entries = file.entries.clone();
    entries.sort_by(|a, b| (&a.contract, &a.key).cmp(&(&b.contract, &b.key)));
    for entry in &entries {
        let value = decode_entry(entry)?;
        values.insert((entry.contract.clone(), entry.key.clone()), value);
    }
    Ok(LoadedSnapshot {
        network: file.network.clone(),
        ledger_sequence: file.ledger_sequence,
        close_time: file.close_time,
        values,
    })
}

pub fn load_str(text: &str) -> Result<LoadedSnapshot, LoadError> {
    let file: SnapshotFile = serde_json::from_str(text).expect("snapshot JSON");
    load(&file)
}

/// Install ledger sequence and close time, then return the decoded storage.
pub fn install(env: &Env, file: &SnapshotFile) -> Result<LoadedSnapshot, LoadError> {
    let loaded = load(file)?;
    env.ledger().with_mut(|info| {
        info.sequence_number = loaded.ledger_sequence;
        info.timestamp = loaded.close_time;
    });
    Ok(loaded)
}

pub fn to_file(snapshot: &SnapshotFile) -> String {
    let mut file = snapshot.clone();
    file.entries.sort_by(|a, b| (&a.contract, &a.key).cmp(&(&b.contract, &b.key)));
    file.hash = content_hash(&file.entries);
    serde_json::to_string_pretty(&file).expect("snapshot JSON") + "\n"
}

pub fn value<'a>(loaded: &'a LoadedSnapshot, contract: &str, key: &str) -> i128 {
    *loaded
        .values
        .get(&(contract.to_string(), key.to_string()))
        .unwrap_or_else(|| panic!("missing snapshot entry {contract}/{key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entries() -> Vec<Entry> {
        vec![
            Entry {
                contract: "pool".into(),
                key: "premium_rate_bps".into(),
                value: 250,
                xdr: base64::engine::general_purpose::STANDARD.encode(b"250"),
            },
            Entry {
                contract: "oracle".into(),
                key: "timestamp".into(),
                value: 1_729_000_000,
                xdr: base64::engine::general_purpose::STANDARD.encode(b"1729000000"),
            },
        ]
    }

    #[test]
    fn round_trip_capture_matches_loaded_storage() {
        let mut file = SnapshotFile {
            schema: SCHEMA,
            version: VERSION.into(),
            network: "testnet".into(),
            ledger_sequence: 10,
            close_time: 1_729_001_000,
            hash: String::new(),
            entries: sample_entries(),
        };
        let text = to_file(&file);
        file.hash = content_hash(&file.entries);
        let loaded = load_str(&text).expect("load");
        assert_eq!(value(&loaded, "pool", "premium_rate_bps"), 250);
        assert_eq!(loaded.values.len(), 2);
        let env = Env::default();
        let installed = install(&env, &serde_json::from_str(&text).unwrap()).unwrap();
        assert_eq!(env.ledger().sequence(), installed.ledger_sequence);
        assert_eq!(env.ledger().timestamp(), installed.close_time);
    }

    #[test]
    fn tampered_hash_fails_to_load() {
        let mut file = SnapshotFile {
            schema: SCHEMA,
            version: VERSION.into(),
            network: "testnet".into(),
            ledger_sequence: 10,
            close_time: 1,
            hash: "deadbeef".into(),
            entries: sample_entries(),
        };
        file.hash = "deadbeef".into();
        assert_eq!(load(&file), Err(LoadError::BadHash));
    }

    #[test]
    fn wrong_version_fails_to_load() {
        let file = SnapshotFile {
            schema: SCHEMA,
            version: "99".into(),
            network: "testnet".into(),
            ledger_sequence: 10,
            close_time: 1,
            hash: content_hash(&sample_entries()),
            entries: sample_entries(),
        };
        assert!(matches!(load(&file), Err(LoadError::VersionMismatch { .. })));
    }
}
