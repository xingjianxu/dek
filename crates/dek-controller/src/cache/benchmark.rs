//! Measure the two representations of a complete, typed cache record.

use std::{error::Error, hint::black_box, num::NonZeroUsize, time::Instant};

use dek_core::{HostConfig, UnitSpec};
use serde::{
    Serialize,
    de::{DeserializeOwned, IgnoredAny},
};

use crate::{eval::TargetAnswer, modules::ModuleOutput};

use super::{FORMAT_VERSION, OwnedRecord, UnitKey, UnitStage, WireRecord, decode_messagepack};

#[derive(Serialize)]
pub struct RecordComparison {
    format_version: u32,
    key: UnitKey,
    dependencies: usize,
    msgpack: EncodingSample,
    json: EncodingSample,
}

#[derive(Serialize)]
struct EncodingSample {
    bytes: usize,
    encode_ns: u128,
    decode_ns: u128,
}

pub fn compare_cache_records(
    msgpack: &[u8],
    json: &[u8],
    iterations: NonZeroUsize,
) -> Result<RecordComparison, Box<dyn Error>> {
    let header: WireRecord<UnitKey, IgnoredAny, IgnoredAny> =
        decode_messagepack(msgpack).ok_or("invalid MessagePack cache record")?;
    if header.format_version != FORMAT_VERSION {
        return Err(format!("unsupported cache format {}", header.format_version).into());
    }
    let iterations = iterations.get();
    match header.key.stage {
        UnitStage::Collect => compare::<UnitSpec>(msgpack, json, iterations),
        UnitStage::Merge => compare::<HostConfig>(msgpack, json, iterations),
        UnitStage::Expand => compare::<ModuleOutput>(msgpack, json, iterations),
        UnitStage::TargetSnapshot => {
            compare::<Result<TargetAnswer, String>>(msgpack, json, iterations)
        }
    }
}

fn compare<T: DeserializeOwned + Serialize>(
    msgpack: &[u8],
    json: &[u8],
    iterations: usize,
) -> Result<RecordComparison, Box<dyn Error>> {
    let record: OwnedRecord<T> =
        decode_messagepack(msgpack).ok_or("invalid typed MessagePack record")?;
    let json_record: OwnedRecord<T> = serde_json::from_slice(json)?;
    if serde_json::to_value(&record)? != serde_json::to_value(&json_record)? {
        return Err(format!("decoded cache records differ for {:?}", record.key).into());
    }

    // Both decoders were just exercised by the equivalence check.
    black_box(rmp_serde::to_vec(&record)?);
    black_box(serde_json::to_vec(&record)?);
    let pack = sample(
        msgpack,
        iterations,
        || rmp_serde::to_vec(black_box(&record)),
        |data| decode_messagepack::<OwnedRecord<T>>(data).ok_or("invalid MessagePack cache record"),
    )?;
    let json = sample(
        json,
        iterations,
        || serde_json::to_vec(black_box(&record)),
        |data| serde_json::from_slice::<OwnedRecord<T>>(data),
    )?;
    Ok(RecordComparison {
        format_version: record.format_version,
        key: record.key,
        dependencies: record.entry.dependencies.len(),
        msgpack: pack,
        json,
    })
}

fn sample<T, EncodeError: Error + 'static, DecodeError: Into<Box<dyn Error>>>(
    data: &[u8],
    iterations: usize,
    encode: impl Fn() -> Result<Vec<u8>, EncodeError>,
    decode: impl Fn(&[u8]) -> Result<T, DecodeError>,
) -> Result<EncodingSample, Box<dyn Error>> {
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(encode()?);
    }
    let encode_ns = start.elapsed().as_nanos();
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(decode(black_box(data)).map_err(Into::into)?);
    }
    let decode_ns = start.elapsed().as_nanos();
    Ok(EncodingSample {
        bytes: data.len(),
        encode_ns,
        decode_ns,
    })
}
