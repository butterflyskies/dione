//! Path-aware JSON decoding at Teams wire and durable boundaries.

use serde::de::DeserializeOwned;

pub(super) fn decode_json<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, serde_path_to_error::Error<serde_json::Error>> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut deserializer)?;
    deserializer.end().map_err(|source| {
        serde_path_to_error::Error::new(serde_path_to_error::Track::new().path(), source)
    })?;
    Ok(value)
}
