# Module-owned MCP registration

Teams owns its tools/list definition, typed argument parsing, reply-authority invocation, and receipt projection. MCP owns an immutable startup registry with validated names and explicit resident exposure. The registry reserves all legacy and harness-callable names, including callable names not advertised in tools/list.

The public with_teams_reply API is removed; callers compose teams::mcp_tools and DioneServer::with_module_tools. Startup retains concrete ToolNameError and RegistrationError. All repository integration callers migrate in this change. This is an intentional pre-1.0 breaking API change.

The registry does not replace domain actions with a global JSON execution interface. JSON remains at the MCP wire boundary; reply authority and execution remain with Teams. Resident discovery and accepted-operation cancellation semantics are preserved.

Review found that derived struct parsing accepted positional arrays. A production tools/call regression against pre-fix source observed one authority invocation where zero was required. The owner now requires an object before parsing. Expanded tests cover every non-object JSON root, exact error framing without input contents, zero authority calls, and a successful paired object request. Name tests cover accepted128-byte and rejected129-byte limits.

Independent six-lens source reviews found no remaining introduced issue after corrections. Native checks and signed-head gate will be recorded in the PR. Local agent review is not official Forgejo approval.

Inherited library missing-docs enforcement and documentation are assigned to a dedicated standards unit after public contracts stabilize. Typed Teams identities, neutral delivery/actions, and durable admission extraction remain separate vertical units.
Native feature Clippy and all 258 focused MCP/Teams/socket integration tests passed before the version-only bump to 0.65.0. Logs: /home/syn/code/dione-module-r4-clippy.log and /home/syn/code/dione-module-r4-focused.log. Signed-head full verification remains required before publication. Version 0.64.0 is reserved for the separately reviewed scheduler unit that precedes this unit in integration.
