//! Text extraction from uploaded files.

use crate::BrainError;
use async_trait::async_trait;

/// Errors from model calls on media.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// No model is configured for this kind of file.
    #[error("no model is configured for {0}")]
    NotConfigured(&'static str),
    /// The call failed.
    #[error("the model call failed: {0}")]
    Failed(String),
}

/// Reads what text extraction cannot.
#[async_trait]
pub trait MediaReader: Send + Sync {
    /// Describes an image in words.
    async fn describe_image(&self, content_type: &str, bytes: &[u8]) -> Result<String, MediaError>;

    /// Transcribes audio.
    async fn transcribe(
        &self,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, MediaError>;
}

fn is_text(content_type: &str, name: &str) -> bool {
    content_type.starts_with("text/")
        || ["md", "txt", "csv", "json"]
            .iter()
            .any(|extension| name.to_lowercase().ends_with(&format!(".{extension}")))
}

/// Extracts the text of a file.
pub(crate) async fn extract_text(
    content_type: &str,
    name: &str,
    bytes: &[u8],
    media: Option<&dyn MediaReader>,
) -> Result<String, BrainError> {
    if content_type == "application/pdf" || name.to_lowercase().ends_with(".pdf") {
        let bytes = bytes.to_vec();
        return tokio::task::spawn_blocking(move || pdf_extract::extract_text_from_mem(&bytes))
            .await
            .map_err(|error| BrainError::Invalid(error.to_string()))?
            .map(|text| text.trim().to_owned())
            .map_err(|error| BrainError::Invalid(format!("cannot read the PDF: {error}")));
    }
    if content_type.starts_with("image/") {
        let media = media.ok_or(MediaError::NotConfigured("images"))?;
        return Ok(media.describe_image(content_type, bytes).await?);
    }
    if content_type.starts_with("audio/") {
        let media = media.ok_or(MediaError::NotConfigured("audio"))?;
        return Ok(media.transcribe(name, content_type, bytes).await?);
    }
    if is_text(content_type, name) {
        return String::from_utf8(bytes.to_vec())
            .map_err(|_| BrainError::Invalid("the file is not valid UTF-8".to_owned()));
    }
    Err(BrainError::Invalid(format!(
        "unsupported file type `{content_type}`"
    )))
}
