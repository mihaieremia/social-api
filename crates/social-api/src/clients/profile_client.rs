use shared::errors::AppError;
use shared::types::AuthenticatedUser;
use uuid::Uuid;

/// Trait for token validation — transport-swappable.
#[async_trait::async_trait]
pub trait TokenValidator: Send + Sync {
    /// Validate a bearer token and return the authenticated user.
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError>;
}

/// HTTP implementation of TokenValidator.
pub struct HttpTokenValidator {
    http_client: reqwest::Client,
    profile_api_url: String,
}

impl HttpTokenValidator {
    pub fn new(http_client: reqwest::Client, profile_api_url: String) -> Self {
        Self {
            http_client,
            profile_api_url,
        }
    }
}

#[async_trait::async_trait]
impl TokenValidator for HttpTokenValidator {
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
        let url = format!("{}/v1/auth/validate", self.profile_api_url);

        let start = std::time::Instant::now();
        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        let latency = start.elapsed().as_secs_f64();

        let response = match response {
            Ok(r) => {
                metrics::counter!(
                    "social_api_external_calls_total",
                    "service" => "profile_api",
                    "method" => "validate",
                    "status" => r.status().as_u16().to_string(),
                )
                .increment(1);
                metrics::histogram!(
                    "social_api_external_call_duration_seconds",
                    "service" => "profile_api",
                    "method" => "validate",
                )
                .record(latency);
                r
            }
            Err(e) => {
                metrics::counter!(
                    "social_api_external_calls_total",
                    "service" => "profile_api",
                    "method" => "validate",
                    "status" => "error",
                )
                .increment(1);
                metrics::histogram!(
                    "social_api_external_call_duration_seconds",
                    "service" => "profile_api",
                    "method" => "validate",
                )
                .record(latency);
                tracing::error!(
                    service = "profile_api",
                    error = %e,
                    "Profile API request failed"
                );
                return Err(AppError::DependencyUnavailable("profile_api".to_string()));
            }
        };

        if !response.status().is_success() {
            return Err(AppError::Unauthorized(
                "Invalid or expired token".to_string(),
            ));
        }

        let body: serde_json::Value = response.json().await.map_err(|e| {
            tracing::error!(service = "profile_api", error = %e, "Failed to parse profile response");
            AppError::Internal("Failed to parse profile response".to_string())
        })?;

        let valid = body.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);
        if !valid {
            return Err(AppError::Unauthorized(
                "Token validation returned invalid".to_string(),
            ));
        }

        // Parse user_id - strip "usr_" prefix from mock API format
        let user_id_str = body
            .get("user_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::Internal("Missing user_id in profile response".to_string()))?;

        let uuid_str = user_id_str.strip_prefix("usr_").unwrap_or(user_id_str);
        let user_id = Uuid::parse_str(uuid_str)
            .map_err(|_| AppError::Internal(format!("Invalid user_id format: {user_id_str}")))?;

        let display_name = body
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        Ok(AuthenticatedUser {
            user_id,
            display_name,
        })
    }
}
