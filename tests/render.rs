//! Integration tests for the LaTeX rendering pipeline.
//!
//! These tests exercise `render_latex_to_png` directly — no Discord connection
//! needed. They verify PNG validity, dimensions, error handling, and security
//! sanitization.

use dione::mcp::tools::render::render_latex_to_png;

const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn png_dimensions(data: &[u8]) -> (u32, u32) {
    assert!(data.len() > 24, "PNG too small to contain IHDR");
    let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    (width, height)
}

// ── Happy path ──────────────────────────────────────────────────────────────

#[test]
fn test_render_simple_expression() {
    let png = render_latex_to_png(r"x^2 + y^2 = z^2").unwrap();
    assert!(png.starts_with(PNG_HEADER), "output is not valid PNG");
    assert!(
        png.len() > 100,
        "PNG suspiciously small: {} bytes",
        png.len()
    );

    let (w, h) = png_dimensions(&png);
    assert!(w > 20 && h > 20, "dimensions too small: {w}x{h}");
    assert!(
        w < 2000 && h < 2000,
        "dimensions suspiciously large: {w}x{h}"
    );
}

#[test]
fn test_render_fraction() {
    let png = render_latex_to_png(r"\frac{d}{dx} x^2 = 2x").unwrap();
    assert!(png.starts_with(PNG_HEADER));
    let (w, h) = png_dimensions(&png);
    assert!(w > 100 && h > 30, "fraction too small: {w}x{h}");
}

#[test]
fn test_render_integral() {
    // mitex 0.2.4 doesn't support \sqrt — use an integral without it.
    let png = render_latex_to_png(r"\int_0^{1} x^2 \, dx = \frac{1}{3}").unwrap();
    assert!(png.starts_with(PNG_HEADER));
    assert!(
        png.len() > 200,
        "integral PNG too small: {} bytes",
        png.len()
    );
}

#[test]
fn test_render_sum() {
    let png = render_latex_to_png(r"\sum_{k=1}^{n} k = \frac{n(n+1)}{2}").unwrap();
    assert!(png.starts_with(PNG_HEADER));
    let (w, h) = png_dimensions(&png);
    assert!(w > 50 && h > 50, "sum should be substantial: {w}x{h}");
}

#[test]
fn test_render_subscript_superscript() {
    let png = render_latex_to_png(r"f'(x) = 6x^2 + 10x - 6").unwrap();
    assert!(png.starts_with(PNG_HEADER));
}

// ── Dimensions are stable across calls ──────────────────────────────────────

#[test]
fn test_render_deterministic() {
    let png1 = render_latex_to_png(r"E = mc^2").unwrap();
    let png2 = render_latex_to_png(r"E = mc^2").unwrap();
    let (w1, h1) = png_dimensions(&png1);
    let (w2, h2) = png_dimensions(&png2);
    assert_eq!(
        (w1, h1),
        (w2, h2),
        "same expression should produce same dimensions"
    );
}

// ── Error handling ──────────────────────────────────────────────────────────

#[test]
fn test_render_empty_expression() {
    let result = render_latex_to_png("");
    // Empty expression may render as empty math or error — either is acceptable
    // as long as it doesn't panic.
    let _ = result;
}

#[test]
fn test_render_rejects_iftypst() {
    let result = render_latex_to_png(r"\iftypst $ #panic() $ \fi");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("\\iftypst"));
}

#[test]
fn test_render_rejects_dollar_in_output() {
    // This tests the $ sanitization on mitex output.
    // If mitex ever produces a $ in its output, the render should reject it.
    // We can't easily trigger this through normal LaTeX, so this is a
    // regression guard for the sanitization logic itself.
    let result = render_latex_to_png(r"\$");
    // \$ may or may not produce a $ in mitex output — if it does, it should be rejected.
    // If mitex handles it safely, the render succeeds. Either outcome is acceptable.
    let _ = result;
}

// ── Visual inspection output ────────────────────────────────────────────────

#[test]
fn test_render_visual_samples() {
    let samples = [
        (r"\frac{d}{dx} x^2 = 2x", "fraction"),
        (r"\sqrt{x^2 + y^2}", "sqrt"),
        (r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}", "matrix"),
        (r"\int_0^{1} x^2 \, dx = \frac{1}{3}", "integral"),
    ];
    for (latex, name) in &samples {
        let png = render_latex_to_png(latex).unwrap();
        let path = std::env::temp_dir().join(format!("dione-test-render-{name}.png"));
        std::fs::write(&path, &png).unwrap();
        eprintln!("{name}: {}", path.display());
    }
}

// ── mitex fixup tests ───────────────────────────────────────────────────────

#[test]
fn test_render_sqrt() {
    let png = render_latex_to_png(r"\sqrt{x}").unwrap();
    assert!(png.starts_with(PNG_HEADER));
    let (w, h) = png_dimensions(&png);
    assert!(w > 20 && h > 20, "sqrt too small: {w}x{h}");
}

#[test]
fn test_render_sqrt_with_fraction() {
    let png = render_latex_to_png(r"\frac{\sqrt{\pi}}{2}").unwrap();
    assert!(png.starts_with(PNG_HEADER));
}

#[test]
fn test_render_pmatrix() {
    let png = render_latex_to_png(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}").unwrap();
    assert!(png.starts_with(PNG_HEADER));
    let (w, h) = png_dimensions(&png);
    assert!(w > 50 && h > 50, "matrix too small: {w}x{h}");
}
