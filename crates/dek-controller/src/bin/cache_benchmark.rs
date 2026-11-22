use std::{env, error::Error, fs, num::NonZeroUsize};

use dek_controller::cache::compare_cache_records;

fn main() -> Result<(), Box<dyn Error>> {
    let [msgpack, json, iterations]: [_; 3] = env::args_os()
        .skip(1)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| "usage: dek-cache-benchmark MSGPACK_RECORD JSON_RECORD ITERATIONS")?;
    let iterations = iterations
        .to_str()
        .ok_or("invalid iteration count")?
        .parse::<NonZeroUsize>()?;
    let comparison = compare_cache_records(&fs::read(msgpack)?, &fs::read(json)?, iterations)?;
    println!("{}", serde_json::to_string(&comparison)?);
    Ok(())
}
