//! Types and constants for the decryption PIR subsystem.
//!
//! Each leaf in the decryption PIR database corresponds to a single Orchard
//! action and stores the fields needed (alongside `cmx` from the witness PIR)
//! to reconstruct a [`CompactAction`] for trial decryption:
//!
//! - `nf`: the action's input nullifier, used as `rho` for the output note
//! - `ephemeral_key`: Diffie-Hellman public key for note encryption key agreement
//! - `ciphertext`: first 52 bytes of `enc_ciphertext` (compact note plaintext)
//!
//! The database shares the same sub-shard geometry as the witness PIR
//! (256 leaves per sub-shard, 256 sub-shards per shard) but with larger
//! per-leaf entries (116 bytes vs 32 bytes for witness).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use witness_types::{
    decompose_position, physical_row_index, Hash, L0_DB_ROWS, L0_MAX_SHARDS, SHARD_HEIGHT,
    SHARD_LEAVES, SUBSHARD_HEIGHT, SUBSHARD_LEAVES, SUBSHARDS_PER_SHARD,
};

/// Bytes per decryption leaf: nf (32) + ephemeral_key (32) + ciphertext (52).
pub const DECRYPT_LEAF_BYTES: usize = 32 + 32 + 52; // 116

/// Bytes per PIR database row (one sub-shard): 256 leaves x 116 bytes.
pub const DECRYPT_ROW_BYTES: usize = SUBSHARD_LEAVES * DECRYPT_LEAF_BYTES; // 29,696

/// Total PIR database rows (same count as witness: 32 shards x 256 sub-shards).
pub const DECRYPT_DB_ROWS: usize = L0_DB_ROWS; // 8,192

/// Total PIR database size in bytes.
pub const DECRYPT_DB_BYTES: usize = DECRYPT_DB_ROWS * DECRYPT_ROW_BYTES; // ~232 MB

/// A single decryption PIR leaf — the data needed to trial-decrypt one Orchard
/// action when paired with `cmx` from the witness PIR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptionLeaf {
    /// The action's input nullifier (32 bytes). Used as `rho` when
    /// reconstructing the output note via `CompactAction::from_parts`.
    pub nf: [u8; 32],
    /// Ephemeral public key for Diffie-Hellman key agreement (32 bytes).
    pub ephemeral_key: [u8; 32],
    /// First 52 bytes of `enc_ciphertext` — the compact note plaintext
    /// (diversifier + value + rseed + tag).
    #[serde(
        serialize_with = "serialize_ciphertext",
        deserialize_with = "deserialize_ciphertext"
    )]
    pub ciphertext: [u8; 52],
}

fn serialize_ciphertext<S: Serializer>(ct: &[u8; 52], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bytes(ct)
}

fn deserialize_ciphertext<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 52], D::Error> {
    let v: Vec<u8> = Deserialize::deserialize(d)?;
    v.try_into()
        .map_err(|v: Vec<u8>| serde::de::Error::invalid_length(v.len(), &"52 bytes"))
}

impl DecryptionLeaf {
    pub const BYTES: usize = DECRYPT_LEAF_BYTES;

    /// An all-zeros leaf used for padding empty sub-shard positions.
    pub const EMPTY: Self = Self {
        nf: [0u8; 32],
        ephemeral_key: [0u8; 32],
        ciphertext: [0u8; 52],
    };

    /// Serialize to a fixed-size byte array: nf || ephemeral_key || ciphertext.
    pub fn to_bytes(&self) -> [u8; DECRYPT_LEAF_BYTES] {
        let mut buf = [0u8; DECRYPT_LEAF_BYTES];
        buf[..32].copy_from_slice(&self.nf);
        buf[32..64].copy_from_slice(&self.ephemeral_key);
        buf[64..].copy_from_slice(&self.ciphertext);
        buf
    }

    /// Deserialize from a byte slice. Returns `None` if the slice is shorter
    /// than [`DECRYPT_LEAF_BYTES`]. Trailing bytes beyond that are ignored.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < DECRYPT_LEAF_BYTES {
            return None;
        }
        let mut nf = [0u8; 32];
        let mut ephemeral_key = [0u8; 32];
        let mut ciphertext = [0u8; 52];
        nf.copy_from_slice(&bytes[..32]);
        ephemeral_key.copy_from_slice(&bytes[32..64]);
        ciphertext.copy_from_slice(&bytes[64..DECRYPT_LEAF_BYTES]);
        Some(Self {
            nf,
            ephemeral_key,
            ciphertext,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_consistency() {
        assert_eq!(DECRYPT_LEAF_BYTES, 116);
        assert_eq!(DECRYPT_ROW_BYTES, 256 * 116);
        assert_eq!(DECRYPT_ROW_BYTES, 29_696);
        assert_eq!(DECRYPT_DB_ROWS, 8_192);
        assert_eq!(DECRYPT_DB_BYTES, 8_192 * 29_696);
        assert_eq!(DECRYPT_DB_ROWS, L0_MAX_SHARDS * SUBSHARDS_PER_SHARD);
    }

    #[test]
    fn leaf_byte_roundtrip() {
        let leaf = DecryptionLeaf {
            nf: [0xAA; 32],
            ephemeral_key: [0xBB; 32],
            ciphertext: [0xCC; 52],
        };
        let bytes = leaf.to_bytes();
        assert_eq!(bytes.len(), DECRYPT_LEAF_BYTES);
        assert_eq!(&bytes[..32], &[0xAA; 32]);
        assert_eq!(&bytes[32..64], &[0xBB; 32]);
        assert_eq!(&bytes[64..], &[0xCC; 52]);

        let decoded = DecryptionLeaf::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, leaf);
    }

    #[test]
    fn from_bytes_too_short() {
        assert!(DecryptionLeaf::from_bytes(&[0u8; 115]).is_none());
        assert!(DecryptionLeaf::from_bytes(&[]).is_none());
    }

    #[test]
    fn from_bytes_extra_ignored() {
        let mut buf = [0u8; 120];
        buf[..32].copy_from_slice(&[1u8; 32]);
        buf[32..64].copy_from_slice(&[2u8; 32]);
        buf[64..116].copy_from_slice(&[3u8; 52]);
        buf[116..].copy_from_slice(&[0xFF; 4]);

        let leaf = DecryptionLeaf::from_bytes(&buf).unwrap();
        assert_eq!(leaf.nf, [1u8; 32]);
        assert_eq!(leaf.ephemeral_key, [2u8; 32]);
        assert_eq!(leaf.ciphertext, [3u8; 52]);
    }

    #[test]
    fn empty_leaf_is_zeroed() {
        let empty = DecryptionLeaf::EMPTY;
        assert_eq!(empty.nf, [0u8; 32]);
        assert_eq!(empty.ephemeral_key, [0u8; 32]);
        assert_eq!(empty.ciphertext, [0u8; 52]);
        assert_eq!(empty.to_bytes(), [0u8; DECRYPT_LEAF_BYTES]);
    }

    #[test]
    fn serde_json_roundtrip() {
        let leaf = DecryptionLeaf {
            nf: {
                let mut b = [0u8; 32];
                b[0] = 0x42;
                b
            },
            ephemeral_key: {
                let mut b = [0u8; 32];
                b[31] = 0xFF;
                b
            },
            ciphertext: {
                let mut b = [0u8; 52];
                b[0] = 0x01;
                b[51] = 0x99;
                b
            },
        };
        let json = serde_json::to_string(&leaf).unwrap();
        let decoded: DecryptionLeaf = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, leaf);
    }

    #[test]
    fn leaf_bytes_const_matches_struct() {
        assert_eq!(
            DecryptionLeaf::BYTES,
            std::mem::size_of::<[u8; 32]>()
                + std::mem::size_of::<[u8; 32]>()
                + std::mem::size_of::<[u8; 52]>(),
        );
    }

    #[test]
    fn shared_geometry_re_exports() {
        assert_eq!(SUBSHARD_LEAVES, 256);
        assert_eq!(SUBSHARDS_PER_SHARD, 256);
        assert_eq!(SHARD_LEAVES, 65_536);
        assert_eq!(L0_DB_ROWS, 8_192);
        assert_eq!(L0_MAX_SHARDS, 32);

        let (s, ss, l) = decompose_position((2u64 << 16) | (100u64 << 8) | 50);
        assert_eq!((s, ss, l), (2, 100, 50));
        assert_eq!(physical_row_index(2, 100, 0), 2 * 256 + 100);
    }
}
