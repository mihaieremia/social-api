use shared::errors::AppError;
use shared::types::BatchItem;

use crate::config::Config;

/// Reject unknown content types early. Returns `ContentTypeUnknown` if the
/// type has no `CONTENT_API_{TYPE}_URL` configured.
pub fn ensure_registered_content_type(config: &Config, content_type: &str) -> Result<(), AppError> {
    if !config.is_valid_content_type(content_type) {
        return Err(AppError::ContentTypeUnknown(content_type.to_string()));
    }

    Ok(())
}

/// Validate all content types in an iterator, failing on the first unknown type.
pub fn ensure_registered_content_types<'a>(
    config: &Config,
    content_types: impl IntoIterator<Item = &'a str>,
) -> Result<(), AppError> {
    for content_type in content_types {
        ensure_registered_content_type(config, content_type)?;
    }

    Ok(())
}

/// Validate all content types in a batch request's items.
pub fn ensure_registered_batch_items(config: &Config, items: &[BatchItem]) -> Result<(), AppError> {
    ensure_registered_content_types(config, items.iter().map(|item| item.content_type.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_registered_content_type_accepts_known_value() {
        let config = Config::new_for_test();
        assert!(ensure_registered_content_type(&config, "post").is_ok());
    }

    #[test]
    fn ensure_registered_content_type_rejects_unknown_value() {
        let config = Config::new_for_test();
        let err = ensure_registered_content_type(&config, "unknown").expect_err("must fail");
        assert!(matches!(err, AppError::ContentTypeUnknown(_)));
    }
}
