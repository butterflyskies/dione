# Oh My Pi integration

Use the interactive Oh My Pi 18.10.0 main session. Configure its MCP server named exactly `dione` to launch your Dione executable with its existing arguments plus `--mode oh-my-pi`. For example, add this entry to the construct's `.omp/mcp.json`, using its own executable and isolated state paths:

```json
{
  "mcpServers": {
    "dione": {
      "type": "stdio",
      "command": "/absolute/path/to/dione",
      "args": ["--mode", "oh-my-pi"],
      "env": {"DIONE_STATE_DIR": "/absolute/path/to/construct-state"}
    }
  }
}
```

Keep the existing Discord credential setup. The extension needs no credentials.
Use the original extension file from this Dione checkout; `cargo install` installs
the executable but does not install OMP extensions. Load this file explicitly:

```text
omp -e /absolute/path/to/dione/extensions/oh-my-pi/dione.ts
```

Keep one active main session per Dione MCP connection. Ordinary MCP discovery exposes Dione's tools, and their existing access checks govern replies. Optional Teams delivery uses the saved-session bridge described below.

The extension accepts `notifications/dione/event` from `dione` with the strict envelope `{version: 1, content: string, meta: object}`. Metadata supports nested JSON values, including evidence arrays, batches, booleans, and nulls. The entire envelope permits at most 128 nested object or array containers. Cycles and non-JSON values are rejected. Diagnostic events need no `chat_id`.

Accepted events become a displayed `dione-event` custom message. Its model-visible text contains fixed external-data guidance followed by the complete JSON envelope. The submission requests `triggerTurn: true` and `deliverAs: "followUp"`. Only `ctx.agent.kind === "main"` receives it; a child at depth zero is still a child.

Before provider context is built, both `dione-event` and `dione.teams.receipt` custom messages become user-role messages with their original content. This also applies to saved history when Teams delivery is disabled. Other extensions' custom messages stay unchanged. The conversion avoids OMP's default developer-role treatment of custom messages; it does not elevate external conversation into instructions. The saved custom-message details remain unchanged, and the complete generic metadata stays in the model-visible JSON envelope.

For the generic event path, `sendMessage` returns void. Submission proves neither model receipt nor completion. There is no acknowledgement, retry, replay, durable queue, or ordering guarantee. Disconnects, upstream buffer overflow, and session changes can lose events. The external-data label is provenance guidance, not a prompt-injection guarantee. ACP and collaboration guest turn behavior are not supported by this verification.

## Saved-session Teams delivery

Configure Dione's Teams listener and explicit tenant/conversation admission, then launch the OMP child with its private resident-control socket. Set `DIONE_TEAMS_CONTROL_SOCKET` in the **OMP host environment** to that absolute socket path. Its parent must be owned by the same Unix user and mode 0700; the socket must be mode 0600. The extension rejects Windows named pipes and nonprivate sockets. Keep the socket out of model-visible tool arguments.

Set `DIONE_TEAMS_SESSION_STORAGE=file` in the OMP host environment only when using OMP's local `FileSessionStorage`. OMP 18.10's readonly extension API exposes no authoritative backend discriminator. This explicit setting declares the supported storage configuration; it does not bypass checks of the actual saved JSONL, native file identity, header, or SDK entries. Other backends remain paused.

The main session must already have a saved transcript. OMP allocates a path before it creates the file; the bridge waits visibly until the file exists. It does not call nonexistent extension flush APIs or change OMP's storage implementation.

Teams notifications exclusively enter the typed Teams bridge. Exact native tenant, bot, conversation, and activity identities form a SHA-256 receipt key. The bridge requests one displayed `dione.teams.receipt` custom message, with native identity details and the saved session ID. A successful private ACK requires that exact receipt in both the actual complete saved transcript and current SDK history, followed by file synchronization and unchanged file/session identity. Linux also synchronizes the containing directory. Receipt means durable assistant admission, not inference success or a reply.

A renewed delivery token never triggers a second submission while the first is unresolved. Because `sendMessage` returns void, a silent OMP preflight failure cannot be distinguished from a submission still in flight. The bridge reports that it awaits a transcript, withholds ACK, and vetoes session transitions. It does not retry on a timer. Recovery requires resolving persistence or stopping the old OMP host before restarting it; never run two hosts against the same saved session. Dione retains unacknowledged events for replay. Legacy receipt details that lack the exact session/native tuple are unsupported and pause the bridge; they are not silently accepted as durable evidence.

Teams reply handles expire 15 minutes after their authority is minted. Durable event replay does not renew that authority, so a retained event may arrive after its reply handle expires. Replying with an expired handle fails before sending, with the runtime error `UnknownReplyHandle`, also used for unknown or consumed handles. The tool reports an error rather than a successful reply.

Session-change hooks cancel promptly while delivery or binding work remains active; they never wait for that work inside OMP's lifecycle timeout. The first change request pauses delivery and starts revocation in the background. Retry after it finishes. A lost bind response remains uncertain because Dione may already have bound the session. Recovery must rebind that exact saved session and confirm revocation; a failed socket exchange never grants permission to change sessions. If recovery remains unavailable, close the old Dione MCP child before restarting the host.

`teams.test.ts` exercises the bridge using real OMP `SessionManager` files and recording control/submission boundaries. It covers saved-file admission, receipt ACKs, duplicate replay, renewed leases, silent submission, malformed identities, incomplete/altered transcripts, and late bind shutdown. It does not prove a Unix socket exchange, a Dione child, inference, or a live Teams round trip. Those require the separate native integration gate and an authorized live check.

## Verify with the published runner

`dione.test.ts` is original offline test code. It uses the actual OMP loader and `ExtensionRunner`, local empty authentication storage, and recording runtime actions. It makes no model or Discord call. Keep dependencies in a separate proof directory outside Dione.

In that directory, install the exact test target and copy only this test file:

```powershell
bun add --ignore-scripts --exact @oh-my-pi/pi-coding-agent@18.10.0
Copy-Item /absolute/path/to/dione/extensions/oh-my-pi/dione.test.ts ./dione.test.ts
$env:DIONE_EXTENSION_PATH = '/absolute/path/to/dione/extensions/oh-my-pi/dione.ts'
bun test ./dione.test.ts
```

Run from the external proof directory. The fixture creates a `dione-runner-*` directory there for its empty auth database. Seven tests cover startup buffering, main and child contexts, schema rejection, hostile content, complete metadata, and separate runners. Idle and streaming contexts verify the requested submission options; they do not run inference.

The tested package corresponds to upstream commit `b83200eb866821f02625332025b453f06ce4e12a`. Its `pi.zod` facade lacks recursive JSON combinators and accepts arrays in `record`. The extension therefore uses a bounded recursive JSON predicate and explicitly rejects array metadata. A precheck also avoids a pinned facade error formatter that throws on BigInt input. See the pinned [MCP configuration guide](https://github.com/can1357/oh-my-pi/blob/b83200eb866821f02625332025b453f06ce4e12a/docs/mcp-config.md) and [extension guide](https://github.com/can1357/oh-my-pi/blob/b83200eb866821f02625332025b453f06ce4e12a/docs/extensions.md).

## Construct validation before main

The local extension proof was run on Windows with Bun 1.4.2. Validate the target
construct's OMP runtime separately. Confirm that a real Dione notification reaches
OMP through its MCP connection and produces one dione-event, then exercise an
allowed reply and a denied action with the dedicated test bot and isolated state.

Dione's access checks govern Dione tools. They do not constrain OMP's shell or
file tools. Review the construct's OMP tool-approval policy before admitting
external events that can trigger a turn. Multiple top-level listeners can submit
the same event more than once. An edit or deletion does not retract an event that
OMP has already received.
