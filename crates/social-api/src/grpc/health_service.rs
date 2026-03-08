//! gRPC health check service implementation.
//!
//! Implements `social.v1.Health.Check` with the same dependency set as HTTP readiness.

use std::collections::HashMap;

use tonic::{Request, Response, Status};

use crate::health;
use crate::proto::social_v1;
use crate::proto::social_v1::health_server::Health;
use crate::state::AppState;

pub struct GrpcHealthService {
    state: AppState,
}

impl GrpcHealthService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl Health for GrpcHealthService {
    async fn check(
        &self,
        _req: Request<social_v1::HealthCheckRequest>,
    ) -> Result<Response<social_v1::HealthCheckResponse>, Status> {
        let mut details = HashMap::new();
        let health = health::check_dependencies(&self.state).await;

        details.insert(
            "database".to_string(),
            if health.database { "ok" } else { "error" }.to_string(),
        );
        details.insert(
            "redis".to_string(),
            if health.redis { "ok" } else { "error" }.to_string(),
        );
        details.insert(
            "content_api".to_string(),
            if health.content_api { "ok" } else { "error" }.to_string(),
        );

        let status = if health.all_healthy() {
            social_v1::ServingStatus::Serving
        } else {
            social_v1::ServingStatus::NotServing
        };

        Ok(Response::new(social_v1::HealthCheckResponse {
            status: status as i32,
            details,
        }))
    }
}
