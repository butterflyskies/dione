//! JSON projections of retained render results and typed rendering failures.

use crate::{FileRenderError, FileRenderErrorKind, PersistedImage, RenderError, RenderErrorKind};
use serde_json::{Value, json};

/// Projects a persisted file or its original rendering and persistence diagnostic.
pub fn file(result: Result<PersistedImage, FileRenderError>) -> Value {
    match result {
        Ok(image) => {
            json!({"ok": true, "path": image.path().to_string_lossy(), "size_bytes": image.size_bytes()})
        }
        Err(error) => match error.kind {
            FileRenderErrorKind::Render(error) => render_error(error),
            FileRenderErrorKind::Create(error) => {
                json!({"error": format!("failed to create temp file: {error}")})
            }
            FileRenderErrorKind::Write { source, .. } => {
                json!({"error": format!("failed to write PNG: {source}")})
            }
            FileRenderErrorKind::Persist(error) => {
                json!({"error": format!("failed to persist temp file: {error}")})
            }
        },
    }
}

/// Projects a rendering diagnostic while preserving its existing display contract.
pub fn render_error(error: RenderError) -> Value {
    let message = match error.kind {
        RenderErrorKind::IfTypstUnsupported => "\\iftypst is not supported".to_string(),
        RenderErrorKind::Parse(error) => format!("LaTeX parse error: {error}"),
        RenderErrorKind::UnsafeTypstOutput => "expression produces unsafe typst output".to_string(),
        RenderErrorKind::Compile(error) => format!("typst compile error: {error}"),
        RenderErrorKind::NoPages => "typst produced no pages".to_string(),
        RenderErrorKind::Encode(error) => format!("PNG encode error: {error}"),
    };
    json!({"error": message})
}
