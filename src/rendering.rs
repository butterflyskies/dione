//! LaTeX compilation and retained PNG files.

use std::{
    io,
    path::{Path, PathBuf},
};
use thiserror::Error;
use typst::layout::PagedDocument;
use typst_as_lib::{TypstAsLibError, TypstEngine};

#[derive(Debug)]
pub(crate) struct PngImage {
    bytes: Vec<u8>,
}

impl PngImage {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug)]
pub(crate) struct PersistedImage {
    path: PathBuf,
    size_bytes: usize,
}

impl PersistedImage {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn size_bytes(&self) -> usize {
        self.size_bytes
    }
}

#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct RenderError {
    #[source]
    pub(crate) kind: RenderErrorKind,
}

#[derive(Debug, Error)]
pub(crate) enum RenderErrorKind {
    #[error("\\iftypst is not supported")]
    IfTypstUnsupported,
    #[error("latex parse error: {0}")]
    Parse(String),
    #[error("expression produces unsafe typst output")]
    UnsafeTypstOutput,
    #[error("typst compile error: {0}")]
    Compile(#[source] TypstAsLibError),
    #[error("typst produced no pages")]
    NoPages,
    #[error("png encode error: {0}")]
    Encode(#[source] png::EncodingError),
}

#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct FileRenderError {
    #[source]
    pub(crate) kind: FileRenderErrorKind,
}

#[derive(Debug, Error)]
pub(crate) enum FileRenderErrorKind {
    #[error(transparent)]
    Render(RenderError),
    #[error("failed to create temp file: {0}")]
    Create(#[source] io::Error),
    #[error("failed to write png to `{path}`: {source}", path = path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to persist temp file: {0}")]
    Persist(#[source] tempfile::PathPersistError),
}

const MATH_TEMPLATE: &str = r##"
#set page(width: auto, height: auto, margin: 4pt)
#set text(size: 16pt, fill: rgb("#e0e0e0"))
$<<<MATH_CONTENT>>>$
"##;

const PIXEL_PER_PT: f32 = 3.0;

/// Render a LaTeX expression to PNG bytes.
pub(crate) fn render_png(latex: &str) -> Result<PngImage, RenderError> {
    if latex.contains(r"\iftypst") {
        return Err(RenderError {
            kind: RenderErrorKind::IfTypstUnsupported,
        });
    }

    let typst_math = mitex::convert_math(latex, None).map_err(|source| RenderError {
        kind: RenderErrorKind::Parse(source),
    })?;

    if typst_math.contains('$') {
        return Err(RenderError {
            kind: RenderErrorKind::UnsafeTypstOutput,
        });
    }

    let typst_math = fixup_mitex_output(&typst_math);
    render_typst_math_to_png(&typst_math)
}

/// Fix known mitex output quirks.
///
/// mitex 0.2.4 has a bug where `convert_math` ignores custom `CommandSpec`
/// during alias resolution (always uses DEFAULT_SPEC). These fixups correct
/// the known wrong aliases until the upstream bug is fixed.
fn fixup_mitex_output(typst_math: &str) -> String {
    typst_math
        .replace("mitexsqrt(", "sqrt(")
        .replace("pmatrix(", "mat(")
        .replace("bmatrix(", "mat(delim: \"[\",")
        .replace("vmatrix(", "mat(delim: \"|\",")
        .replace("Bmatrix(", "mat(delim: \"{\",")
}

fn render_typst_math_to_png(typst_math: &str) -> Result<PngImage, RenderError> {
    let source = MATH_TEMPLATE.replace("<<<MATH_CONTENT>>>", typst_math);

    // TypstEngine binds the changing source at construction.
    let engine = TypstEngine::builder()
        .main_file(source.as_str())
        .fonts(typst_assets::fonts())
        .build();

    let result = engine.compile::<PagedDocument>();

    let doc = result.output.map_err(|source| RenderError {
        kind: RenderErrorKind::Compile(source),
    })?;

    if doc.pages.is_empty() {
        return Err(RenderError {
            kind: RenderErrorKind::NoPages,
        });
    }

    let pixmap = typst_render::render(&doc.pages[0], PIXEL_PER_PT);
    pixmap
        .encode_png()
        .map(|bytes| PngImage { bytes })
        .map_err(|source| RenderError {
            kind: RenderErrorKind::Encode(source),
        })
}

pub(crate) fn render_file(latex: &str) -> Result<PersistedImage, FileRenderError> {
    let png = render_png(latex).map_err(|source| FileRenderError {
        kind: FileRenderErrorKind::Render(source),
    })?;
    let tmp = tempfile::Builder::new()
        .prefix("dione-latex-")
        .suffix(".png")
        .tempfile()
        .map_err(|source| FileRenderError {
            kind: FileRenderErrorKind::Create(source),
        })?;
    std::fs::write(tmp.path(), png.as_bytes()).map_err(|source| FileRenderError {
        kind: FileRenderErrorKind::Write {
            path: tmp.path().to_path_buf(),
            source,
        },
    })?;
    let path = tmp
        .into_temp_path()
        .keep()
        .map_err(|source| FileRenderError {
            kind: FileRenderErrorKind::Persist(source),
        })?;
    Ok(PersistedImage {
        path,
        size_bytes: png.bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use sha2::{Digest, Sha256};

    #[derive(Deserialize)]
    struct PngOracle {
        expression: String,
        bytes: usize,
        sha256: String,
    }

    #[test]
    fn render_contract_png_fingerprints() {
        let oracles: Vec<PngOracle> =
            serde_json::from_str(include_str!("../tests/fixtures/render-oracles.json")).unwrap();
        assert_eq!(oracles.len(), 8);
        for oracle in oracles {
            let png_image = render_png(&oracle.expression).unwrap();
            let png = png_image.as_bytes();
            assert_eq!(png.len(), oracle.bytes, "{}", oracle.expression);
            assert_eq!(
                format!("{:x}", Sha256::digest(png)),
                oracle.sha256,
                "{}",
                oracle.expression,
            );
        }
    }

    const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    fn png_dimensions(data: &[u8]) -> (u32, u32) {
        assert!(data.len() > 24, "PNG too small to contain IHDR");
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        (width, height)
    }

    #[test]
    fn test_render_simple_expression() {
        let png_image = render_png(r"x^2 + y^2 = z^2").unwrap();
        let png = png_image.as_bytes();
        assert!(png.starts_with(PNG_HEADER), "output is not valid PNG");
        assert!(
            png.len() > 100,
            "PNG suspiciously small: {} bytes",
            png.len()
        );

        let (w, h) = png_dimensions(png);
        assert!(w > 20 && h > 20, "dimensions too small: {w}x{h}");
        assert!(
            w < 2000 && h < 2000,
            "dimensions suspiciously large: {w}x{h}"
        );
    }

    #[test]
    fn test_render_fraction() {
        let png_image = render_png(r"\frac{d}{dx} x^2 = 2x").unwrap();
        let png = png_image.as_bytes();
        assert!(png.starts_with(PNG_HEADER));
        let (w, h) = png_dimensions(png);
        assert!(w > 100 && h > 30, "fraction too small: {w}x{h}");
    }

    #[test]
    fn test_render_integral() {
        let png_image = render_png(r"\int_0^{1} x^2 \, dx = \frac{1}{3}").unwrap();
        let png = png_image.as_bytes();
        assert!(png.starts_with(PNG_HEADER));
        assert!(
            png.len() > 200,
            "integral PNG too small: {} bytes",
            png.len()
        );
    }

    #[test]
    fn test_render_sum() {
        let png_image = render_png(r"\sum_{k=1}^{n} k = \frac{n(n+1)}{2}").unwrap();
        let png = png_image.as_bytes();
        assert!(png.starts_with(PNG_HEADER));
        let (w, h) = png_dimensions(png);
        assert!(w > 50 && h > 50, "sum should be substantial: {w}x{h}");
    }

    #[test]
    fn test_render_subscript_superscript() {
        let png_image = render_png(r"f'(x) = 6x^2 + 10x - 6").unwrap();
        let png = png_image.as_bytes();
        assert!(png.starts_with(PNG_HEADER));
    }

    #[test]
    fn test_render_deterministic() {
        let png1_image = render_png(r"E = mc^2").unwrap();
        let png1 = png1_image.as_bytes();
        let png2_image = render_png(r"E = mc^2").unwrap();
        let png2 = png2_image.as_bytes();
        let (w1, h1) = png_dimensions(png1);
        let (w2, h2) = png_dimensions(png2);
        assert_eq!(
            (w1, h1),
            (w2, h2),
            "same expression should produce same dimensions"
        );
    }

    #[test]
    fn test_render_rejects_iftypst() {
        let result = render_png(r"\iftypst $ #panic() $ \fi");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("\\iftypst"));
    }

    #[test]
    fn test_render_sqrt() {
        for expression in [r"\sqrt{x}", r"\sqrt{x^2 + y^2}"] {
            let png_image = render_png(expression).unwrap();
            let png = png_image.as_bytes();
            assert!(png.starts_with(PNG_HEADER));
            let (w, h) = png_dimensions(png);
            assert!(w > 20 && h > 20, "sqrt too small: {w}x{h}");
        }
    }

    #[test]
    fn test_render_sqrt_with_fraction() {
        let png_image = render_png(r"\frac{\sqrt{\pi}}{2}").unwrap();
        let png = png_image.as_bytes();
        assert!(png.starts_with(PNG_HEADER));
    }

    #[test]
    fn test_render_pmatrix() {
        let png_image = render_png(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}").unwrap();
        let png = png_image.as_bytes();
        assert!(png.starts_with(PNG_HEADER));
        let (w, h) = png_dimensions(png);
        assert!(w > 50 && h > 50, "matrix too small: {w}x{h}");
    }

    #[test]
    fn render_contract_compiler_rejection_diagnostics() {
        assert_eq!(
            render_png(r"\iftypst").unwrap_err().to_string(),
            "\\iftypst is not supported"
        );
        let error = render_png(r"\frac{").unwrap_err().to_string();
        assert!(
            error.starts_with("typst compile error: Typst source error:"),
            "{error}"
        );
        assert!(error.contains("missing argument: denom"), "{error}");
        let escaped_dollar_image = render_png(r"\$").unwrap();
        let escaped_dollar = escaped_dollar_image.as_bytes();
        assert!(escaped_dollar.starts_with(PNG_HEADER));
    }

    #[test]
    fn persisted_receipt_drop_keeps_file() {
        let receipt = render_file("x").unwrap();
        let path = receipt.path().to_path_buf();
        let size_bytes = receipt.size_bytes();
        drop(receipt);
        let contents = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(contents.len(), size_bytes);
        assert!(contents.starts_with(PNG_HEADER));
    }
}
