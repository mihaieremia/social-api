use uuid::Uuid;

/// Known content types. Any valid UUID is accepted for these types.
/// Adding a new content type requires only a CONTENT_API_{TYPE}_URL env var
/// in the main service — and adding it here for mock validation.
const KNOWN_CONTENT_TYPES: &[&str] = &["post", "bonus_hunter", "top_picks"];

/// Check if a content type is registered.
pub fn is_known_content_type(content_type: &str) -> bool {
    KNOWN_CONTENT_TYPES.contains(&content_type)
}

/// Pre-seeded valid content IDs per content type (backward-compatible).
/// For stress testing, use `is_known_content_type()` instead — it accepts any UUID.
#[allow(dead_code)]
pub fn content_ids(content_type: &str) -> Vec<Uuid> {
    match content_type {
        "post" => vec![
            // Original 5
            Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap(),
            Uuid::parse_str("9601c044-6130-4ee5-a155-96570e05a02f").unwrap(),
            Uuid::parse_str("933dde0f-4744-4a66-9a38-bf5cb1f67553").unwrap(),
            Uuid::parse_str("ea0f2020-0509-45fd-adb9-24b8843055ee").unwrap(),
            Uuid::parse_str("bd27f926-0a00-41fd-b085-a7491e6d0902").unwrap(),
            // Extended 15
            Uuid::parse_str("a0000001-0001-4000-8000-000000000001").unwrap(),
            Uuid::parse_str("a0000001-0002-4000-8000-000000000002").unwrap(),
            Uuid::parse_str("a0000001-0003-4000-8000-000000000003").unwrap(),
            Uuid::parse_str("a0000001-0004-4000-8000-000000000004").unwrap(),
            Uuid::parse_str("a0000001-0005-4000-8000-000000000005").unwrap(),
            Uuid::parse_str("a0000001-0006-4000-8000-000000000006").unwrap(),
            Uuid::parse_str("a0000001-0007-4000-8000-000000000007").unwrap(),
            Uuid::parse_str("a0000001-0008-4000-8000-000000000008").unwrap(),
            Uuid::parse_str("a0000001-0009-4000-8000-000000000009").unwrap(),
            Uuid::parse_str("a0000001-000a-4000-8000-00000000000a").unwrap(),
            Uuid::parse_str("a0000001-000b-4000-8000-00000000000b").unwrap(),
            Uuid::parse_str("a0000001-000c-4000-8000-00000000000c").unwrap(),
            Uuid::parse_str("a0000001-000d-4000-8000-00000000000d").unwrap(),
            Uuid::parse_str("a0000001-000e-4000-8000-00000000000e").unwrap(),
            Uuid::parse_str("a0000001-000f-4000-8000-00000000000f").unwrap(),
        ],
        "bonus_hunter" => vec![
            // Original 5
            Uuid::parse_str("c3d4e5f6-a7b8-9012-cdef-123456789012").unwrap(),
            Uuid::parse_str("d4e5f6a7-b8c9-0123-def0-234567890123").unwrap(),
            Uuid::parse_str("e5f6a7b8-c9d0-1234-ef01-345678901234").unwrap(),
            Uuid::parse_str("f6a7b8c9-d0e1-2345-f012-456789012345").unwrap(),
            Uuid::parse_str("a7b8c9d0-e1f2-3456-0123-567890123456").unwrap(),
            // Extended 15
            Uuid::parse_str("b0000002-0001-4000-8000-000000000001").unwrap(),
            Uuid::parse_str("b0000002-0002-4000-8000-000000000002").unwrap(),
            Uuid::parse_str("b0000002-0003-4000-8000-000000000003").unwrap(),
            Uuid::parse_str("b0000002-0004-4000-8000-000000000004").unwrap(),
            Uuid::parse_str("b0000002-0005-4000-8000-000000000005").unwrap(),
            Uuid::parse_str("b0000002-0006-4000-8000-000000000006").unwrap(),
            Uuid::parse_str("b0000002-0007-4000-8000-000000000007").unwrap(),
            Uuid::parse_str("b0000002-0008-4000-8000-000000000008").unwrap(),
            Uuid::parse_str("b0000002-0009-4000-8000-000000000009").unwrap(),
            Uuid::parse_str("b0000002-000a-4000-8000-00000000000a").unwrap(),
            Uuid::parse_str("b0000002-000b-4000-8000-00000000000b").unwrap(),
            Uuid::parse_str("b0000002-000c-4000-8000-00000000000c").unwrap(),
            Uuid::parse_str("b0000002-000d-4000-8000-00000000000d").unwrap(),
            Uuid::parse_str("b0000002-000e-4000-8000-00000000000e").unwrap(),
            Uuid::parse_str("b0000002-000f-4000-8000-00000000000f").unwrap(),
        ],
        "top_picks" => vec![
            // Original 5
            Uuid::parse_str("b8c9d0e1-f2a3-4567-1234-678901234567").unwrap(),
            Uuid::parse_str("c9d0e1f2-a3b4-5678-2345-789012345678").unwrap(),
            Uuid::parse_str("d0e1f2a3-b4c5-6789-3456-890123456789").unwrap(),
            Uuid::parse_str("e1f2a3b4-c5d6-7890-4567-901234567890").unwrap(),
            Uuid::parse_str("f2a3b4c5-d6e7-8901-5678-012345678901").unwrap(),
            // Extended 15
            Uuid::parse_str("c0000003-0001-4000-8000-000000000001").unwrap(),
            Uuid::parse_str("c0000003-0002-4000-8000-000000000002").unwrap(),
            Uuid::parse_str("c0000003-0003-4000-8000-000000000003").unwrap(),
            Uuid::parse_str("c0000003-0004-4000-8000-000000000004").unwrap(),
            Uuid::parse_str("c0000003-0005-4000-8000-000000000005").unwrap(),
            Uuid::parse_str("c0000003-0006-4000-8000-000000000006").unwrap(),
            Uuid::parse_str("c0000003-0007-4000-8000-000000000007").unwrap(),
            Uuid::parse_str("c0000003-0008-4000-8000-000000000008").unwrap(),
            Uuid::parse_str("c0000003-0009-4000-8000-000000000009").unwrap(),
            Uuid::parse_str("c0000003-000a-4000-8000-00000000000a").unwrap(),
            Uuid::parse_str("c0000003-000b-4000-8000-00000000000b").unwrap(),
            Uuid::parse_str("c0000003-000c-4000-8000-00000000000c").unwrap(),
            Uuid::parse_str("c0000003-000d-4000-8000-00000000000d").unwrap(),
            Uuid::parse_str("c0000003-000e-4000-8000-00000000000e").unwrap(),
            Uuid::parse_str("c0000003-000f-4000-8000-00000000000f").unwrap(),
        ],
        _ => vec![],
    }
}

/// Validate a token dynamically. Supports:
/// - Static tok_user_1..20 with original UUIDs (backward-compatible)
/// - Dynamic tok_user_21..100000 with generated UUIDs (stress testing)
///
/// Returns (user_id, display_name) on success.
pub fn validate_token(token: &str) -> Option<(String, String)> {
    // Check static entries first (backward compat for tok_user_1..20)
    for entry in static_tokens() {
        if entry.token == token {
            return Some((entry.user_id.to_string(), entry.display_name.to_string()));
        }
    }

    // Dynamic: tok_user_N for any N in 1..=100000
    let n: u64 = token.strip_prefix("tok_user_")?.parse().ok()?;
    if !(1..=100_000).contains(&n) {
        return None;
    }
    let user_id = format!("00000000-0000-4000-8000-{n:012x}");
    let display_name = format!("Test User {n}");
    Some((user_id, display_name))
}

/// Static token entries — backward-compatible with original 20 test users.
pub struct TokenEntry {
    pub token: &'static str,
    pub user_id: &'static str,
    pub display_name: &'static str,
}

fn static_tokens() -> Vec<TokenEntry> {
    vec![
        TokenEntry {
            token: "tok_user_1",
            user_id: "550e8400-e29b-41d4-a716-446655440001",
            display_name: "Test User 1",
        },
        TokenEntry {
            token: "tok_user_2",
            user_id: "550e8400-e29b-41d4-a716-446655440002",
            display_name: "Test User 2",
        },
        TokenEntry {
            token: "tok_user_3",
            user_id: "550e8400-e29b-41d4-a716-446655440003",
            display_name: "Test User 3",
        },
        TokenEntry {
            token: "tok_user_4",
            user_id: "550e8400-e29b-41d4-a716-446655440004",
            display_name: "Test User 4",
        },
        TokenEntry {
            token: "tok_user_5",
            user_id: "550e8400-e29b-41d4-a716-446655440005",
            display_name: "Test User 5",
        },
        TokenEntry {
            token: "tok_user_6",
            user_id: "550e8400-e29b-41d4-a716-446655440006",
            display_name: "Test User 6",
        },
        TokenEntry {
            token: "tok_user_7",
            user_id: "550e8400-e29b-41d4-a716-446655440007",
            display_name: "Test User 7",
        },
        TokenEntry {
            token: "tok_user_8",
            user_id: "550e8400-e29b-41d4-a716-446655440008",
            display_name: "Test User 8",
        },
        TokenEntry {
            token: "tok_user_9",
            user_id: "550e8400-e29b-41d4-a716-446655440009",
            display_name: "Test User 9",
        },
        TokenEntry {
            token: "tok_user_10",
            user_id: "550e8400-e29b-41d4-a716-446655440010",
            display_name: "Test User 10",
        },
        TokenEntry {
            token: "tok_user_11",
            user_id: "550e8400-e29b-41d4-a716-446655440011",
            display_name: "Test User 11",
        },
        TokenEntry {
            token: "tok_user_12",
            user_id: "550e8400-e29b-41d4-a716-446655440012",
            display_name: "Test User 12",
        },
        TokenEntry {
            token: "tok_user_13",
            user_id: "550e8400-e29b-41d4-a716-446655440013",
            display_name: "Test User 13",
        },
        TokenEntry {
            token: "tok_user_14",
            user_id: "550e8400-e29b-41d4-a716-446655440014",
            display_name: "Test User 14",
        },
        TokenEntry {
            token: "tok_user_15",
            user_id: "550e8400-e29b-41d4-a716-446655440015",
            display_name: "Test User 15",
        },
        TokenEntry {
            token: "tok_user_16",
            user_id: "550e8400-e29b-41d4-a716-446655440016",
            display_name: "Test User 16",
        },
        TokenEntry {
            token: "tok_user_17",
            user_id: "550e8400-e29b-41d4-a716-446655440017",
            display_name: "Test User 17",
        },
        TokenEntry {
            token: "tok_user_18",
            user_id: "550e8400-e29b-41d4-a716-446655440018",
            display_name: "Test User 18",
        },
        TokenEntry {
            token: "tok_user_19",
            user_id: "550e8400-e29b-41d4-a716-446655440019",
            display_name: "Test User 19",
        },
        TokenEntry {
            token: "tok_user_20",
            user_id: "550e8400-e29b-41d4-a716-446655440020",
            display_name: "Test User 20",
        },
    ]
}

/// Backward-compatible: returns static token entries only.
/// Callers that need dynamic token support should use `validate_token()` instead.
#[allow(dead_code)]
pub fn tokens() -> Vec<TokenEntry> {
    static_tokens()
}
