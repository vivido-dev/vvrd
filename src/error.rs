use thiserror::Error;

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("mupdf error: {0}")]
    Mupdf(#[from] mupdf::error::Error),
    #[error("document has no pages")]
    EmptyDocument,
    #[error("invalid document: {0}")]
    InvalidDocument(String),
    #[error("conversion error: {0}")]
    Converting(String),
    #[error("markup error: {0}")]
    Markup(String),
    #[error("rendering page {} panicked: {message}", page + 1)]
    Panicked { page: usize, message: String },
}
