use uuid::Uuid;

/// Pre-seeded valid content IDs per content type (5 each per spec).
pub fn content_ids(content_type: &str) -> Vec<Uuid> {
    match content_type {
        "post" => vec![
            Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap(),
            Uuid::parse_str("9601c044-6130-4ee5-a155-96570e05a02f").unwrap(),
            Uuid::parse_str("933dde0f-4744-4a66-9a38-bf5cb1f67553").unwrap(),
            Uuid::parse_str("ea0f2020-0509-45fd-adb9-24b8843055ee").unwrap(),
            Uuid::parse_str("bd27f926-0a00-41fd-b085-a7491e6d0902").unwrap(),
        ],
        "bonus_hunter" => vec![
            Uuid::parse_str("c3d4e5f6-a7b8-9012-cdef-123456789012").unwrap(),
            Uuid::parse_str("d4e5f6a7-b8c9-0123-def0-234567890123").unwrap(),
            Uuid::parse_str("e5f6a7b8-c9d0-1234-ef01-345678901234").unwrap(),
            Uuid::parse_str("f6a7b8c9-d0e1-2345-f012-456789012345").unwrap(),
            Uuid::parse_str("a7b8c9d0-e1f2-3456-0123-567890123456").unwrap(),
        ],
        "top_picks" => vec![
            Uuid::parse_str("b8c9d0e1-f2a3-4567-1234-678901234567").unwrap(),
            Uuid::parse_str("c9d0e1f2-a3b4-5678-2345-789012345678").unwrap(),
            Uuid::parse_str("d0e1f2a3-b4c5-6789-3456-890123456789").unwrap(),
            Uuid::parse_str("e1f2a3b4-c5d6-7890-4567-901234567890").unwrap(),
            Uuid::parse_str("f2a3b4c5-d6e7-8901-5678-012345678901").unwrap(),
        ],
        _ => vec![],
    }
}

/// Token -> (user_id, display_name) mapping per spec.
pub struct TokenEntry {
    pub token: &'static str,
    pub user_id: &'static str,
    pub display_name: &'static str,
}

pub fn tokens() -> Vec<TokenEntry> {
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
    ]
}
