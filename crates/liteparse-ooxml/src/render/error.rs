//! Renderer error types.

/// Errors that can occur during rendering.
///
/// An empty document is not an error: it produces a single blank page
/// (`render::layout_document`), matching Word's behavior.
#[derive(Debug)]
pub enum RenderError {
    /// The host font system exposes no typeface at all, so there is nothing to
    /// fall back to when a requested family cannot be resolved. Seen on
    /// container images built without any fonts installed.
    NoFontsAvailable,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::NoFontsAvailable => write!(
                f,
                "no fonts available on this system — install at least one font, \
                 or check that fontconfig is configured"
            ),
        }
    }
}

impl std::error::Error for RenderError {}
