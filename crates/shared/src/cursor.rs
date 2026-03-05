use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Cursor for keyset pagination.
///
/// Encodes a (timestamp, id) pair as base64url JSON. This enables efficient
/// index-seekable pagination using `WHERE (created_at, id) < ($ts, $id)`.
///
/// Why cursor over offset:
/// - No row skipping at depth (offset N scans N rows)
/// - Stable under concurrent writes (inserts don't shift pages)
/// - No COUNT(*) needed for pagination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    /// Timestamp component (created_at of last item)
    #[serde(rename = "t")]
    pub timestamp: DateTime<Utc>,
    /// ID component (primary key of last item)
    pub id: i64,
}

impl Cursor {
    /// Create a new cursor from the last item in a page.
    pub fn new(timestamp: DateTime<Utc>, id: i64) -> Self {
        Self { timestamp, id }
    }

    /// Encode cursor to a base64url string.
    pub fn encode(&self) -> String {
        let json = serde_json::to_string(self).expect("cursor serialization cannot fail");
        URL_SAFE_NO_PAD.encode(json.as_bytes())
    }

    /// Decode cursor from a base64url string.
    pub fn decode(encoded: &str) -> Result<Self, CursorError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| CursorError::InvalidBase64)?;

        let json = std::str::from_utf8(&bytes).map_err(|_| CursorError::InvalidUtf8)?;

        serde_json::from_str(json).map_err(|_| CursorError::InvalidFormat)
    }
}

/// Errors that can occur during cursor operations.
#[derive(Debug, thiserror::Error)]
pub enum CursorError {
    #[error("invalid base64 encoding")]
    InvalidBase64,
    #[error("invalid UTF-8 in cursor")]
    InvalidUtf8,
    #[error("invalid cursor format")]
    InvalidFormat,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_cursor_roundtrip() {
        let ts = Utc.with_ymd_and_hms(2026, 2, 2, 17, 0, 0).unwrap();
        let cursor = Cursor::new(ts, 12345);

        let encoded = cursor.encode();
        let decoded = Cursor::decode(&encoded).expect("decode should succeed");

        assert_eq!(decoded.timestamp, ts);
        assert_eq!(decoded.id, 12345);
    }

    #[test]
    fn test_cursor_invalid_base64() {
        assert!(Cursor::decode("not-valid-base64!!!").is_err());
    }

    #[test]
    fn test_cursor_invalid_json() {
        let encoded = URL_SAFE_NO_PAD.encode(b"not json");
        assert!(Cursor::decode(&encoded).is_err());
    }

    #[test]
    fn test_cursor_missing_fields() {
        let json = r#"{"t":"2026-02-02T17:00:00Z"}"#;
        let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());
        assert!(Cursor::decode(&encoded).is_err());
    }
}
