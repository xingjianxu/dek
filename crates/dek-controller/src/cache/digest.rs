use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// SHA-256 digest, displayed as lowercase hexadecimal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CacheDigest(#[serde(with = "serde_bytes")] [u8; 32]);

impl CacheDigest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for CacheDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Hash the caller's semantic projection with recursively sorted object keys.
///
/// Arrays keep their order. Use [canonical_set] in the projection for fields
/// whose members are unordered. The projection also controls whether source
/// locations contribute to the digest.
pub fn semantic_digest(value: &Value) -> CacheDigest {
    let mut canonical = value.clone();
    canonical.sort_all_objects();
    CacheDigest::of_bytes(&encode_json(&canonical))
}

/// Represent a schema-defined set as an array sorted by canonical JSON bytes.
pub fn canonical_set(items: impl IntoIterator<Item = Value>) -> Value {
    let mut items: Vec<_> = items
        .into_iter()
        .map(|mut item| {
            item.sort_all_objects();
            (encode_json(&item), item)
        })
        .collect();

    items.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    items.dedup_by(|(left, _), (right, _)| left == right);
    Value::Array(items.into_iter().map(|(_, item)| item).collect())
}

fn encode_json(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("serializing a JSON value to memory")
}
