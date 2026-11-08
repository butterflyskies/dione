# Dione rendering

`dione-rendering` converts LaTeX expressions to PNG bytes and retained files.
It owns conversion, Typst compilation, bundled fonts, encoding, and persistence.
The wire module owns file-result and rendering-error JSON projection. Discord authorization, attachment delivery, and channel-result projection stay in Dione.

```rust,no_run
let image = dione_rendering::render_png("x^2 + y^2 = z^2")?;
assert!(!image.as_bytes().is_empty());
# Ok::<(), dione_rendering::RenderError>(())
```

Successful image and file receipts have private fields. Rendering and persistence
errors retain their concrete source types, including temporary-path ownership.
