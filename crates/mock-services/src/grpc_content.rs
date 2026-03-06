use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::data;
use crate::proto::internal_v1;
use crate::proto::internal_v1::content_service_server::ContentService;

pub struct MockContentService;

#[tonic::async_trait]
impl ContentService for MockContentService {
    async fn validate(
        &self,
        req: Request<internal_v1::ValidateContentRequest>,
    ) -> Result<Response<internal_v1::ValidateContentResponse>, Status> {
        let body = req.into_inner();

        let content_id = Uuid::parse_str(&body.content_id)
            .map_err(|_| Status::invalid_argument("Invalid content ID"))?;

        let valid_ids = data::content_ids(&body.content_type);
        let exists = valid_ids.contains(&content_id);

        Ok(Response::new(internal_v1::ValidateContentResponse {
            exists,
        }))
    }
}
