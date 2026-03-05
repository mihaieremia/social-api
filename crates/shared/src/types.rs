use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use utoipa::ToSchema;
use uuid::Uuid;

/// Authenticated user identity extracted from token validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticatedUser {
    pub user_id: Uuid,
    pub display_name: String,
}

/// Reference to a specific content item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ContentRef {
    pub content_type: String,
    pub content_id: Uuid,
}

impl fmt::Display for ContentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.content_type, self.content_id)
    }
}

/// Time window for leaderboard queries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum TimeWindow {
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
    #[serde(rename = "all")]
    All,
}

impl TimeWindow {
    /// Parse from string representation.
    pub fn from_str_value(s: &str) -> Option<Self> {
        match s {
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            "30d" => Some(Self::Month),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Returns the duration in seconds for this window, None for All.
    pub fn duration_secs(&self) -> Option<i64> {
        match self {
            Self::Day => Some(86_400),
            Self::Week => Some(604_800),
            Self::Month => Some(2_592_000),
            Self::All => None,
        }
    }

    /// Returns the string representation used in Redis keys and API responses.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::All => "all",
        }
    }
}

impl fmt::Display for TimeWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A like record as stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Like {
    pub id: i64,
    pub user_id: Uuid,
    pub content_type: String,
    pub content_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// SSE event types for the like stream.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "event")]
pub enum LikeEvent {
    #[serde(rename = "like")]
    Liked {
        user_id: Uuid,
        count: i64,
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "unlike")]
    Unliked {
        user_id: Uuid,
        count: i64,
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "heartbeat")]
    Heartbeat { timestamp: DateTime<Utc> },
    #[serde(rename = "shutdown")]
    Shutdown { timestamp: DateTime<Utc> },
}

/// Pagination parameters for cursor-based pagination.
#[derive(Debug, Clone, Deserialize)]
pub struct PaginationParams {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub content_type: Option<String>,
}

/// Paginated response wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct PaginatedResponse<T: Serialize> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// Concrete paginated response for user likes (OpenAPI schema alias).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaginatedUserLikes {
    pub items: Vec<UserLikeItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

impl From<PaginatedResponse<UserLikeItem>> for PaginatedUserLikes {
    fn from(resp: PaginatedResponse<UserLikeItem>) -> Self {
        Self {
            items: resp.items,
            next_cursor: resp.next_cursor,
            has_more: resp.has_more,
        }
    }
}

/// A user's liked item in list responses.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserLikeItem {
    #[schema(example = "post")]
    pub content_type: String,
    #[schema(example = "731b0395-4888-4822-b516-05b4b7bf2089")]
    pub content_id: Uuid,
    pub liked_at: DateTime<Utc>,
}

/// Like count response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LikeCountResponse {
    #[schema(example = "post")]
    pub content_type: String,
    #[schema(example = "731b0395-4888-4822-b516-05b4b7bf2089")]
    pub content_id: Uuid,
    #[schema(example = 42)]
    pub count: i64,
}

/// Like status response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LikeStatusResponse {
    #[schema(example = true)]
    pub liked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liked_at: Option<DateTime<Utc>>,
}

/// Like action response (after like/unlike).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LikeActionResponse {
    #[schema(example = true)]
    pub liked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub already_existed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = true)]
    pub was_liked: Option<bool>,
    #[schema(example = 42)]
    pub count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liked_at: Option<DateTime<Utc>>,
}

/// Batch request item.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BatchItem {
    #[schema(example = "post")]
    pub content_type: String,
    #[schema(example = "731b0395-4888-4822-b516-05b4b7bf2089")]
    pub content_id: Uuid,
}

/// Batch request body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BatchRequest {
    pub items: Vec<BatchItem>,
}

/// Batch count result.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchCountResult {
    pub content_type: String,
    pub content_id: Uuid,
    pub count: i64,
}

/// Batch status result.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchStatusResult {
    pub content_type: String,
    pub content_id: Uuid,
    pub liked: bool,
    pub liked_at: Option<DateTime<Utc>>,
}

/// Top liked content item.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TopLikedItem {
    #[schema(example = "post")]
    pub content_type: String,
    #[schema(example = "731b0395-4888-4822-b516-05b4b7bf2089")]
    pub content_id: Uuid,
    #[schema(example = 1547)]
    pub count: i64,
}

/// Top liked response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TopLikedResponse {
    pub window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub items: Vec<TopLikedItem>,
}

/// Like request body (POST /v1/likes).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LikeRequest {
    #[schema(example = "post")]
    pub content_type: String,
    #[schema(example = "731b0395-4888-4822-b516-05b4b7bf2089")]
    pub content_id: Uuid,
}

/// Health check detail.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HealthDetail {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Health check response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HealthResponse {
    #[schema(example = "ok")]
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<std::collections::HashMap<String, HealthDetail>>,
}

/// Batch counts response wrapper.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchCountsResponse {
    pub results: Vec<BatchCountResult>,
}

/// Batch statuses response wrapper.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchStatusesResponse {
    pub results: Vec<BatchStatusResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_window_from_str() {
        assert_eq!(TimeWindow::from_str_value("24h"), Some(TimeWindow::Day));
        assert_eq!(TimeWindow::from_str_value("7d"), Some(TimeWindow::Week));
        assert_eq!(TimeWindow::from_str_value("30d"), Some(TimeWindow::Month));
        assert_eq!(TimeWindow::from_str_value("all"), Some(TimeWindow::All));
        assert_eq!(TimeWindow::from_str_value("1y"), None);
        assert_eq!(TimeWindow::from_str_value(""), None);
    }

    #[test]
    fn test_time_window_duration() {
        assert_eq!(TimeWindow::Day.duration_secs(), Some(86_400));
        assert_eq!(TimeWindow::Week.duration_secs(), Some(604_800));
        assert_eq!(TimeWindow::Month.duration_secs(), Some(2_592_000));
        assert_eq!(TimeWindow::All.duration_secs(), None);
    }

    #[test]
    fn test_time_window_as_str() {
        assert_eq!(TimeWindow::Day.as_str(), "24h");
        assert_eq!(TimeWindow::Week.as_str(), "7d");
        assert_eq!(TimeWindow::Month.as_str(), "30d");
        assert_eq!(TimeWindow::All.as_str(), "all");
    }

    #[test]
    fn test_time_window_display() {
        assert_eq!(format!("{}", TimeWindow::Day), "24h");
        assert_eq!(format!("{}", TimeWindow::All), "all");
    }

    #[test]
    fn test_content_ref_display() {
        let cr = ContentRef {
            content_type: "post".to_string(),
            content_id: Uuid::parse_str("731b0395-4888-4822-b516-05b4b7bf2089").unwrap(),
        };
        assert_eq!(format!("{cr}"), "post:731b0395-4888-4822-b516-05b4b7bf2089");
    }

    #[test]
    fn test_like_event_serialization() {
        let event = LikeEvent::Liked {
            user_id: Uuid::nil(),
            count: 42,
            timestamp: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"like\""));
        assert!(json.contains("\"count\":42"));
    }

    #[test]
    fn test_like_status_response_hides_null_liked_at() {
        let resp = LikeStatusResponse {
            liked: false,
            liked_at: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("liked_at"));
    }
}
