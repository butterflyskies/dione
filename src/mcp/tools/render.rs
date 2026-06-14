use serde_json::{Value, json};
use serenity::model::id::ChannelId;
use typst::layout::PagedDocument;
use typst_as_lib::TypstEngine;

use crate::mcp::tools::messaging::MessagingCtx;

const MATH_TEMPLATE: &str = r##"
#set page(width: auto, height: auto, margin: 4pt)
#set text(size: 16pt, fill: rgb("#e0e0e0"))
$<<<MATH_CONTENT>>>$
"##;

const PIXEL_PER_PT: f32 = 3.0;

/// Render a LaTeX expression to PNG bytes.
pub fn render_latex_to_png(latex: &str) -> Result<Vec<u8>, String> {
    if latex.contains(r"\iftypst") {
        return Err("\\iftypst is not supported".to_string());
    }

    let typst_math =
        mitex::convert_math(latex, None).map_err(|e| format!("LaTeX parse error: {e}"))?;

    if typst_math.contains('$') {
        return Err("expression produces unsafe typst output".to_string());
    }

    let typst_math = fixup_mitex_output(&typst_math);
    render_typst_math_to_png(&typst_math)
}

/// Fix known mitex output quirks.
///
/// mitex 0.2.4 has a bug where `convert_math` ignores custom `CommandSpec`
/// during alias resolution (always uses DEFAULT_SPEC). These fixups correct
/// the known wrong aliases until the upstream bug is fixed.
/// See: https://github.com/mitex-rs/mitex/issues/XXX
fn fixup_mitex_output(typst_math: &str) -> String {
    typst_math
        .replace("mitexsqrt(", "sqrt(")
        .replace("pmatrix(", "mat(")
        .replace("bmatrix(", "mat(delim: \"[\",")
        .replace("vmatrix(", "mat(delim: \"|\",")
        .replace("Bmatrix(", "mat(delim: \"{\",")
}

/// Render a Typst math expression to PNG bytes.
fn render_typst_math_to_png(typst_math: &str) -> Result<Vec<u8>, String> {
    let source = MATH_TEMPLATE.replace("<<<MATH_CONTENT>>>", typst_math);

    // Build a fresh engine for each render — the source changes each time and
    // TypstEngine binds sources at construction.  Font data comes from
    // embedded byte slices so the cost is acceptable.
    let engine = TypstEngine::builder()
        .main_file(source.as_str())
        .fonts(typst_assets::fonts())
        .build();

    let result = engine.compile::<PagedDocument>();

    let doc = result
        .output
        .map_err(|e| format!("typst compile error: {e}"))?;

    if doc.pages.is_empty() {
        return Err("typst produced no pages".to_string());
    }

    let pixmap = typst_render::render(&doc.pages[0], PIXEL_PER_PT);
    pixmap
        .encode_png()
        .map_err(|e| format!("PNG encode error: {e}"))
}

// ── render_latex tool ────────────────────────────────────────────────────────

pub async fn render_latex(latex: &str) -> Value {
    match render_latex_to_png(latex) {
        Ok(png_bytes) => {
            let tmp = match tempfile::Builder::new()
                .prefix("dione-latex-")
                .suffix(".png")
                .tempfile()
            {
                Ok(t) => t,
                Err(e) => return json!({ "error": format!("failed to create temp file: {e}") }),
            };
            if let Err(e) = std::fs::write(tmp.path(), &png_bytes) {
                return json!({ "error": format!("failed to write PNG: {e}") });
            }
            let path = match tmp.into_temp_path().keep() {
                Ok(p) => p,
                Err(e) => return json!({ "error": format!("failed to persist temp file: {e}") }),
            };
            json!({
                "ok": true,
                "path": path.to_string_lossy(),
                "size_bytes": png_bytes.len(),
            })
        }
        Err(e) => json!({ "error": e }),
    }
}

// ── render_latex_to_channel tool ─────────────────────────────────────────────

pub async fn render_latex_to_channel(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    latex: &str,
    caption: Option<&str>,
) -> Value {
    use serenity::builder::CreateAttachment;

    if let Err(e) = crate::mcp::tools::messaging::check_outbound(ctx, channel_id).await {
        return e;
    }

    let png_bytes = match render_latex_to_png(latex) {
        Ok(bytes) => bytes,
        Err(e) => return json!({ "error": e }),
    };

    let attachment = CreateAttachment::bytes(png_bytes.as_slice(), "math.png");
    crate::mcp::tools::messaging::send_attachment(ctx, channel_id, attachment, caption).await
}
