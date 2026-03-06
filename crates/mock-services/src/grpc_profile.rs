use tonic::{Request, Response, Status};

use crate::data;
use crate::proto::internal_v1;
use crate::proto::internal_v1::profile_service_server::ProfileService;

pub struct MockProfileService;

#[tonic::async_trait]
impl ProfileService for MockProfileService {
    async fn validate_token(
        &self,
        req: Request<internal_v1::ValidateTokenRequest>,
    ) -> Result<Response<internal_v1::ValidateTokenResponse>, Status> {
        let body = req.into_inner();

        for entry in data::tokens() {
            if entry.token == body.token {
                return Ok(Response::new(internal_v1::ValidateTokenResponse {
                    valid: true,
                    user_id: entry.user_id.to_string(),
                    display_name: entry.display_name.to_string(),
                }));
            }
        }

        Err(Status::unauthenticated("Invalid token"))
    }
}
