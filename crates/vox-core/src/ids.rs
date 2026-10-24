//! Identifiers.

/// A new random UUID v4 in lower-case hyphenated form (`crypto.randomUUID()`), used for read ids, session ids and
/// toast ids.
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// True if `id` may name an audio-cache folder: `^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$` (persistence.md §6.1).
pub fn is_valid_read_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(is_valid_read_id(&id));
    }

    #[test]
    fn read_id_rule() {
        assert!(is_valid_read_id("mock-0"));
        assert!(!is_valid_read_id("-x"));
        assert!(!is_valid_read_id("../x"));
        assert!(!is_valid_read_id(""));
        assert!(!is_valid_read_id(&"a".repeat(129)));
    }
}
