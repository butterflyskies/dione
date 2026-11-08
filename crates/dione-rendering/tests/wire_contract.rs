use dione_rendering::{FileRenderError, FileRenderErrorKind, RenderError, RenderErrorKind, wire};
use serde_json::json;
use std::{io, path::PathBuf};

#[test]
fn ordinary_error_projection_retains_categories_and_omits_private_write_path() {
    for (kind, expected) in [
        (
            RenderErrorKind::IfTypstUnsupported,
            "\\iftypst is not supported",
        ),
        (
            RenderErrorKind::Parse("invalid token".into()),
            "LaTeX parse error: invalid token",
        ),
        (
            RenderErrorKind::UnsafeTypstOutput,
            "expression produces unsafe typst output",
        ),
        (RenderErrorKind::NoPages, "typst produced no pages"),
    ] {
        assert_eq!(
            wire::render_error(RenderError { kind }),
            json!({"error":expected})
        );
    }
    assert_eq!(
        wire::file(Err(FileRenderError {
            kind: FileRenderErrorKind::Write {
                path: PathBuf::from("private-path-canary"),
                source: io::Error::other("write fixture"),
            }
        })),
        json!({"error":"failed to write PNG: write fixture"})
    );
    assert_eq!(
        wire::file(Err(FileRenderError {
            kind: FileRenderErrorKind::Render(RenderError {
                kind: RenderErrorKind::IfTypstUnsupported,
            })
        })),
        json!({"error":"\\iftypst is not supported"})
    );
}

#[test]
fn ordinary_persistence_error_keeps_display_and_drops_temporary_path() {
    let temp = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let path = temp.to_path_buf();
    assert!(path.exists());
    assert_eq!(
        wire::file(Err(FileRenderError {
            kind: FileRenderErrorKind::Persist(tempfile::PathPersistError {
                error: io::Error::other("persist fixture"),
                path: temp
            },)
        })),
        json!({"error":"failed to persist temp file: failed to persist temporary file path: persist fixture"})
    );
    assert!(!path.exists());
}

#[test]
fn ordinary_persisted_png_projection_matches_retained_file_bytes() {
    let result = wire::file(dione_rendering::render_file("x"));
    let path = PathBuf::from(result["path"].as_str().unwrap());
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(path.is_absolute());
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(
        result,
        json!({"ok":true,"path":path.to_string_lossy(),"size_bytes":bytes.len()})
    );
}
