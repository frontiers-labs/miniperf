/// The one hash function used everywhere identities are minted: payloads,
/// interned strings, and stack frame lists. Part of the on-disk contract.
pub fn xxh3(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// Identity of an interned string.
pub fn string_hash(value: &str) -> u64 {
    xxh3(value.as_bytes())
}

/// Identity of a deduplicated call stack (leaf first).
pub fn stack_hash(frames: &[u64]) -> u64 {
    let mut bytes = Vec::with_capacity(frames.len() * 8);
    for frame in frames {
        bytes.extend_from_slice(&frame.to_le_bytes());
    }
    xxh3(&bytes)
}
