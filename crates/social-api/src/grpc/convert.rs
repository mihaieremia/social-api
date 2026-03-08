//! Conversion functions between domain types (`shared::types`) and generated
//! proto types (`crate::proto::social_v1`).
//!
//! Every gRPC handler uses these conversions to bridge the transport layer
//! (protobuf) with the service/repository layer (domain types).

#[cfg(test)]
use chrono::TimeZone;
use chrono::{DateTime, Utc};
use prost_types::Timestamp;
use uuid::Uuid;

use crate::proto::social_v1;
use shared::types;

// ── Timestamp helpers ────────────────────────────────────────────────

/// Convert a `DateTime<Utc>` to a `prost_types::Timestamp`.
pub fn to_proto_timestamp(dt: DateTime<Utc>) -> Timestamp {
    Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

/// Convert a `prost_types::Timestamp` to a `DateTime<Utc>`.
///
/// Panics are not possible here because `timestamp_nanos_opt` returns None
/// only for out-of-range values; we fall back to seconds-only precision.
#[cfg(test)]
pub fn from_proto_timestamp(ts: &Timestamp) -> DateTime<Utc> {
    Utc.timestamp_opt(ts.seconds, ts.nanos as u32)
        .single()
        .unwrap_or_else(|| {
            Utc.timestamp_opt(ts.seconds, 0)
                .single()
                .unwrap_or_default()
        })
}

// ── TimeWindow helpers ───────────────────────────────────────────────

/// Convert a domain `TimeWindow` to the proto enum i32 value.
#[cfg(test)]
pub fn to_proto_window(w: types::TimeWindow) -> i32 {
    match w {
        types::TimeWindow::Day => social_v1::TimeWindow::Day as i32,
        types::TimeWindow::Week => social_v1::TimeWindow::Week as i32,
        types::TimeWindow::Month => social_v1::TimeWindow::Month as i32,
        types::TimeWindow::All => social_v1::TimeWindow::All as i32,
    }
}

/// Convert a proto enum i32 value to a domain `TimeWindow`.
///
/// Returns `Err` for `Unspecified` (0) or any unknown value.
pub fn from_proto_window(v: i32) -> Result<types::TimeWindow, &'static str> {
    match v {
        x if x == social_v1::TimeWindow::Day as i32 => Ok(types::TimeWindow::Day),
        x if x == social_v1::TimeWindow::Week as i32 => Ok(types::TimeWindow::Week),
        x if x == social_v1::TimeWindow::Month as i32 => Ok(types::TimeWindow::Month),
        x if x == social_v1::TimeWindow::All as i32 => Ok(types::TimeWindow::All),
        _ => Err("invalid or unspecified time window"),
    }
}

// ── UUID helper ──────────────────────────────────────────────────────

/// Parse a UUID string, returning a `tonic::Status` on failure.
#[allow(clippy::result_large_err)]
pub fn parse_uuid(s: &str) -> Result<Uuid, tonic::Status> {
    Uuid::parse_str(s).map_err(|_| tonic::Status::invalid_argument(format!("invalid UUID: {s}")))
}

// ── ContentRef extraction ────────────────────────────────────────────

/// Extract and validate a list of proto `ContentRef` into `(content_type, content_id)` tuples.
///
/// Returns `tonic::Status::invalid_argument` if any `content_id` is not a valid UUID.
#[allow(clippy::result_large_err)]
pub fn extract_content_refs(
    refs: &[social_v1::ContentRef],
) -> Result<Vec<(String, Uuid)>, tonic::Status> {
    refs.iter()
        .map(|r| {
            let id = parse_uuid(&r.content_id)?;
            Ok((r.content_type.clone(), id))
        })
        .collect()
}

// ── From impls: domain -> proto ──────────────────────────────────────

impl From<types::LikeActionResponse> for social_v1::LikeResponse {
    fn from(r: types::LikeActionResponse) -> Self {
        Self {
            liked: r.liked,
            already_existed: r.already_existed,
            was_liked: r.was_liked,
            count: r.count,
            liked_at: r.liked_at.map(to_proto_timestamp),
        }
    }
}

impl From<types::LikeCountResponse> for social_v1::CountResponse {
    fn from(r: types::LikeCountResponse) -> Self {
        Self {
            content_type: r.content_type,
            content_id: r.content_id.to_string(),
            count: r.count,
        }
    }
}

impl From<types::BatchCountResult> for social_v1::CountResponse {
    fn from(r: types::BatchCountResult) -> Self {
        Self {
            content_type: r.content_type,
            content_id: r.content_id.to_string(),
            count: r.count,
        }
    }
}

impl From<types::BatchStatusResult> for social_v1::StatusResponse {
    fn from(r: types::BatchStatusResult) -> Self {
        Self {
            content_type: r.content_type,
            content_id: r.content_id.to_string(),
            liked: r.liked,
            liked_at: r.liked_at.map(to_proto_timestamp),
        }
    }
}

impl From<types::UserLikeItem> for social_v1::LikeItem {
    fn from(r: types::UserLikeItem) -> Self {
        Self {
            content_type: r.content_type,
            content_id: r.content_id.to_string(),
            liked_at: Some(to_proto_timestamp(r.liked_at)),
        }
    }
}

impl From<types::TopLikedItem> for social_v1::TopLikedItem {
    fn from(r: types::TopLikedItem) -> Self {
        Self {
            content_type: r.content_type,
            content_id: r.content_id.to_string(),
            count: r.count,
        }
    }
}

impl From<types::LikeEvent> for social_v1::LikeEvent {
    fn from(e: types::LikeEvent) -> Self {
        let event = match e {
            types::LikeEvent::Liked {
                user_id,
                count,
                timestamp,
            } => social_v1::like_event::Event::Liked(social_v1::LikeOccurred {
                // content_type and content_id are not carried by the domain event;
                // the caller (gRPC service impl) fills them in from the stream context.
                content_type: String::new(),
                content_id: String::new(),
                count,
                user_id: user_id.to_string(),
                timestamp: Some(to_proto_timestamp(timestamp)),
            }),
            types::LikeEvent::Unliked {
                user_id,
                count,
                timestamp,
            } => social_v1::like_event::Event::Unliked(social_v1::UnlikeOccurred {
                content_type: String::new(),
                content_id: String::new(),
                count,
                user_id: user_id.to_string(),
                timestamp: Some(to_proto_timestamp(timestamp)),
            }),
            types::LikeEvent::Heartbeat { timestamp } => {
                social_v1::like_event::Event::Heartbeat(social_v1::Heartbeat {
                    timestamp: Some(to_proto_timestamp(timestamp)),
                })
            }
            types::LikeEvent::Shutdown { timestamp } => {
                social_v1::like_event::Event::Shutdown(social_v1::Shutdown {
                    timestamp: Some(to_proto_timestamp(timestamp)),
                })
            }
        };
        Self { event: Some(event) }
    }
}

pub fn to_proto_stream_event(
    event: types::LikeEvent,
    content_type: &str,
    content_id: &str,
) -> social_v1::LikeEvent {
    let mut proto: social_v1::LikeEvent = event.into();

    if let Some(event) = &mut proto.event {
        match event {
            social_v1::like_event::Event::Liked(occurred) => {
                occurred.content_type = content_type.to_string();
                occurred.content_id = content_id.to_string();
            }
            social_v1::like_event::Event::Unliked(occurred) => {
                occurred.content_type = content_type.to_string();
                occurred.content_id = content_id.to_string();
            }
            social_v1::like_event::Event::Heartbeat(_)
            | social_v1::like_event::Event::Shutdown(_) => {}
        }
    }

    proto
}

// ── Special conversion: LikeStatusResponse -> StatusResponse ─────────

/// Convert a domain `LikeStatusResponse` to a proto `StatusResponse`.
///
/// The domain type does not carry `content_type` / `content_id`, so the caller
/// must provide them (typically from the request).
pub fn to_proto_status(
    r: types::LikeStatusResponse,
    content_type: String,
    content_id: String,
) -> social_v1::StatusResponse {
    social_v1::StatusResponse {
        content_type,
        content_id,
        liked: r.liked,
        liked_at: r.liked_at.map(to_proto_timestamp),
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    #[test]
    fn timestamp_roundtrip() {
        let dt = Utc.with_ymd_and_hms(2026, 3, 7, 12, 30, 45).unwrap();
        let ts = to_proto_timestamp(dt);
        let back = from_proto_timestamp(&ts);
        assert_eq!(dt, back);
    }

    #[test]
    fn timestamp_roundtrip_with_nanos() {
        let dt = Utc
            .with_ymd_and_hms(2025, 1, 15, 8, 0, 0)
            .unwrap()
            .with_nanosecond(123_456_789)
            .unwrap();
        let ts = to_proto_timestamp(dt);
        assert_eq!(ts.nanos, 123_456_789);
        let back = from_proto_timestamp(&ts);
        assert_eq!(dt, back);
    }

    #[test]
    fn time_window_roundtrip_all_variants() {
        let variants = [
            (types::TimeWindow::Day, social_v1::TimeWindow::Day as i32),
            (types::TimeWindow::Week, social_v1::TimeWindow::Week as i32),
            (
                types::TimeWindow::Month,
                social_v1::TimeWindow::Month as i32,
            ),
            (types::TimeWindow::All, social_v1::TimeWindow::All as i32),
        ];
        for (domain, proto_i32) in variants {
            assert_eq!(to_proto_window(domain), proto_i32);
            assert_eq!(from_proto_window(proto_i32).unwrap(), domain);
        }
    }

    #[test]
    fn time_window_unspecified_is_error() {
        assert!(from_proto_window(social_v1::TimeWindow::Unspecified as i32).is_err());
    }

    #[test]
    fn time_window_unknown_value_is_error() {
        assert!(from_proto_window(999).is_err());
    }

    #[test]
    fn parse_uuid_valid() {
        let id = "731b0395-4888-4822-b516-05b4b7bf2089";
        let result = parse_uuid(id);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Uuid::parse_str(id).unwrap());
    }

    #[test]
    fn parse_uuid_invalid() {
        let result = parse_uuid("not-a-uuid");
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("invalid UUID"));
    }

    #[test]
    fn like_action_response_conversion() {
        let liked_at = Utc::now();
        let domain = types::LikeActionResponse {
            liked: true,
            already_existed: Some(false),
            was_liked: None,
            count: 42,
            liked_at: Some(liked_at),
        };
        let proto: social_v1::LikeResponse = domain.into();
        assert!(proto.liked);
        assert_eq!(proto.already_existed, Some(false));
        assert_eq!(proto.was_liked, None);
        assert_eq!(proto.count, 42);
        assert!(proto.liked_at.is_some());
        assert_eq!(from_proto_timestamp(&proto.liked_at.unwrap()), liked_at);
    }

    #[test]
    fn like_count_response_conversion() {
        let id = Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap();
        let domain = types::LikeCountResponse {
            content_type: "post".to_string(),
            content_id: id,
            count: 100,
        };
        let proto: social_v1::CountResponse = domain.into();
        assert_eq!(proto.content_type, "post");
        assert_eq!(proto.content_id, id.to_string());
        assert_eq!(proto.count, 100);
    }

    #[test]
    fn batch_count_result_conversion() {
        let id = Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap();
        let domain = types::BatchCountResult {
            content_type: "article".to_string(),
            content_id: id,
            count: 55,
        };
        let proto: social_v1::CountResponse = domain.into();
        assert_eq!(proto.content_type, "article");
        assert_eq!(proto.content_id, id.to_string());
        assert_eq!(proto.count, 55);
    }

    #[test]
    fn batch_status_result_conversion() {
        let id = Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap();
        let liked_at = Utc::now();
        let domain = types::BatchStatusResult {
            content_type: "post".to_string(),
            content_id: id,
            liked: true,
            liked_at: Some(liked_at),
        };
        let proto: social_v1::StatusResponse = domain.into();
        assert_eq!(proto.content_type, "post");
        assert_eq!(proto.content_id, id.to_string());
        assert!(proto.liked);
        assert!(proto.liked_at.is_some());
    }

    #[test]
    fn user_like_item_conversion() {
        let id = Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap();
        let liked_at = Utc::now();
        let domain = types::UserLikeItem {
            content_type: "post".to_string(),
            content_id: id,
            liked_at,
        };
        let proto: social_v1::LikeItem = domain.into();
        assert_eq!(proto.content_type, "post");
        assert_eq!(proto.content_id, id.to_string());
        assert!(proto.liked_at.is_some());
        assert_eq!(from_proto_timestamp(&proto.liked_at.unwrap()), liked_at);
    }

    #[test]
    fn top_liked_item_conversion() {
        let id = Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap();
        let domain = types::TopLikedItem {
            content_type: "post".to_string(),
            content_id: id,
            count: 1547,
        };
        let proto: social_v1::TopLikedItem = domain.into();
        assert_eq!(proto.content_type, "post");
        assert_eq!(proto.content_id, id.to_string());
        assert_eq!(proto.count, 1547);
    }

    #[test]
    fn extract_content_refs_valid() {
        let refs = vec![
            social_v1::ContentRef {
                content_type: "post".to_string(),
                content_id: "731b0395-4888-4822-b516-05b4b7bf2089".to_string(),
            },
            social_v1::ContentRef {
                content_type: "article".to_string(),
                content_id: "00000000-0000-0000-0000-000000000000".to_string(),
            },
        ];
        let result = extract_content_refs(&refs).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "post");
        assert_eq!(
            result[0].1,
            Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap()
        );
        assert_eq!(result[1].0, "article");
        assert_eq!(result[1].1, Uuid::nil());
    }

    #[test]
    fn extract_content_refs_invalid_uuid() {
        let refs = vec![social_v1::ContentRef {
            content_type: "post".to_string(),
            content_id: "not-a-uuid".to_string(),
        }];
        let result = extract_content_refs(&refs);
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn like_event_liked_conversion() {
        let user_id = Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap();
        let timestamp = Utc::now();
        let domain = types::LikeEvent::Liked {
            user_id,
            count: 10,
            timestamp,
        };
        let proto: social_v1::LikeEvent = domain.into();
        match proto.event.unwrap() {
            social_v1::like_event::Event::Liked(occurred) => {
                assert_eq!(occurred.user_id, user_id.to_string());
                assert_eq!(occurred.count, 10);
                // content_type/content_id are empty -- caller fills them in
                assert!(occurred.content_type.is_empty());
                assert!(occurred.content_id.is_empty());
                assert_eq!(
                    from_proto_timestamp(&occurred.timestamp.unwrap()),
                    timestamp
                );
            }
            _ => panic!("expected Liked variant"),
        }
    }

    #[test]
    fn like_event_unliked_conversion() {
        let user_id = Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap();
        let timestamp = Utc::now();
        let domain = types::LikeEvent::Unliked {
            user_id,
            count: 9,
            timestamp,
        };
        let proto: social_v1::LikeEvent = domain.into();
        match proto.event.unwrap() {
            social_v1::like_event::Event::Unliked(occurred) => {
                assert_eq!(occurred.user_id, user_id.to_string());
                assert_eq!(occurred.count, 9);
                assert!(occurred.content_type.is_empty());
                assert!(occurred.content_id.is_empty());
            }
            _ => panic!("expected Unliked variant"),
        }
    }

    #[test]
    fn like_event_heartbeat_conversion() {
        let timestamp = Utc::now();
        let domain = types::LikeEvent::Heartbeat { timestamp };
        let proto: social_v1::LikeEvent = domain.into();
        match proto.event.unwrap() {
            social_v1::like_event::Event::Heartbeat(hb) => {
                assert_eq!(from_proto_timestamp(&hb.timestamp.unwrap()), timestamp);
            }
            _ => panic!("expected Heartbeat variant"),
        }
    }

    #[test]
    fn like_event_shutdown_conversion() {
        let timestamp = Utc::now();
        let domain = types::LikeEvent::Shutdown { timestamp };
        let proto: social_v1::LikeEvent = domain.into();
        match proto.event.unwrap() {
            social_v1::like_event::Event::Shutdown(sd) => {
                assert_eq!(from_proto_timestamp(&sd.timestamp.unwrap()), timestamp);
            }
            _ => panic!("expected Shutdown variant"),
        }
    }

    #[test]
    fn to_proto_status_conversion() {
        let liked_at = Utc::now();
        let domain = types::LikeStatusResponse {
            liked: true,
            liked_at: Some(liked_at),
        };
        let proto = to_proto_status(
            domain,
            "post".to_string(),
            "731b0395-4888-4822-b516-05b4b7bf2089".to_string(),
        );
        assert_eq!(proto.content_type, "post");
        assert_eq!(proto.content_id, "731b0395-4888-4822-b516-05b4b7bf2089");
        assert!(proto.liked);
        assert!(proto.liked_at.is_some());
    }

    #[test]
    fn to_proto_status_not_liked() {
        let domain = types::LikeStatusResponse {
            liked: false,
            liked_at: None,
        };
        let proto = to_proto_status(domain, "article".to_string(), "some-id".to_string());
        assert!(!proto.liked);
        assert!(proto.liked_at.is_none());
    }
}
