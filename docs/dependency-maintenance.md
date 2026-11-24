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

## chrono and chrono-tz

[RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543) and
[RUSTSEC-2026-0555](https://rustsec.org/advisories/RUSTSEC-2026-0555) identify
`chrono` 0.4.45 and `chrono-tz` 0.10.4 as unmaintained, with no patched release.
The exact advisory exceptions in `deny.toml` retain this risk while migration
work is pending (#765). They do not report a known vulnerability.

Dione uses `chrono` for message timestamps, delivery scheduling, and
configuration parsing throughout the crate. It parses `chrono-tz` zones from
operator configuration only, not from Discord or agent input.

- [ ] Choose a maintained replacement (`jiff` or `time` with a zone database)
  and confirm that it covers IANA zone parsing and the current output formats.
- [ ] Migrate timestamp, delivery, and configuration code. Verify the rendered
  timestamps and configured-zone behavior against the current tests.
- [ ] Remove both exceptions after `cargo tree` confirms that the crates are
  gone. Run the dependency audit without the exceptions.
