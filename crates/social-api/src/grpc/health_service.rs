//! gRPC health check service implementation.
//!
//! Implements `social.v1.Health.Check` with database and Redis dependency
//! checks, matching the HTTP `/health/ready` endpoint behavior.

use std::collections::HashMap;

use tonic::{Request, Response, Status};

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

        // Run dependency checks in parallel (same pattern as HTTP readiness handler)
        let (db_ok, redis_ok) = tokio::join!(
            self.state.db().is_healthy(),
            self.state.cache().is_healthy(),
        );

        details.insert(
            "database".to_string(),
            if db_ok { "ok" } else { "error" }.to_string(),
        );
        details.insert(
            "redis".to_string(),
            if redis_ok { "ok" } else { "error" }.to_string(),
        );

        let status = if db_ok && redis_ok {
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
