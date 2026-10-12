# Retained dependency maintenance

## unsafe-libyaml

[RUSTSEC-2024-0449](https://rustsec.org/advisories/RUSTSEC-2024-0449)
identifies `unsafe-libyaml` 0.2.11 as unmaintained, with no patched release.
The exact advisory exception in `deny.toml` retains this risk while replacement
work is pending. It does not establish parser safety or remediate the dependency.
Other advisory checks retain their existing policy.

Dione's nameplate loader reaches the parser through `serde_yaml_ng` and accepts
YAML from a configured remote URL. Size checks and output sanitization do not
prove parser memory safety. Typst also retains it through `serde_yaml` in the
dependency graph; this record does not claim a traced Typst execution path.
Replacing only the nameplate parser would leave the Typst dependency path.

- [ ] Replace the nameplate YAML parser with a maintained implementation. Verify
  current remote fixtures and malformed-input behavior.
- [ ] Upgrade or replace the Typst dependency path retaining `serde_yaml` and
  `unsafe-libyaml`. Verify rendering and bibliography behavior.
- [ ] Remove the exception after `cargo tree` confirms both paths are gone.
  Run the dependency audit and full local gate without the exception.

## yoke-derive

The separate `yoke-derive` 0.8.3 yanked-version warning remains unresolved.
This exception does not suppress or remediate it.

- [ ] Inspect the yank reason and parent constraints, then update through the
  dependency chain to a compatible non-yanked release. Run the dependency audit
  and behavior checks for affected consumers.
