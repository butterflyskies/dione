# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Initial implementation: Discord MCP channel server for Claude Code
- Full access control gate (DM allowlist, guild channel opt-in, mention detection)
- 24 MCP tools (messaging, introspection, management, access, bot state)
- Access request queue with admin notifications
- Permission relay with Discord buttons (admin-only routing)
- TOML config with hot-reload
- Message chunking with paragraph-aware splitting
- 77 tests (unit + integration)
