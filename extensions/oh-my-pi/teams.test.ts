import { expect, test } from "bun:test";
import * as zod from "@oh-my-pi/omptype/zod";
import { SessionManager } from "@oh-my-pi/pi-coding-agent/session/session-manager";
import { ExtensionRunner } from "@oh-my-pi/pi-coding-agent/extensibility/extensions/runner";
import { loadExtensions } from "@oh-my-pi/pi-coding-agent/extensibility/extensions/loader";
import { AuthStorage } from "@oh-my-pi/pi-coding-agent/session/auth-storage";
import { ModelRegistry } from "@oh-my-pi/pi-coding-agent/config/model-registry";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { chmod, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createTeamsBridge, receiptFor, teamsSchemas, TEAMS_RECEIPT, transcriptReceipts, type Control } from "./teams";

async function fixture(saved = true, fileStorage = true) {
	const root = await mkdtemp(join(tmpdir(), "dione-teams-extension-"));
	const manager = SessionManager.create(root, join(root, "sessions"));
	if (saved) await manager.ensureOnDisk();
	const notices: string[] = [];
	const sent: Parameters<ExtensionAPI["sendMessage"]>[0][] = [];
	const controls: Parameters<Control>[0][] = [];
	const ctx = { sessionManager: manager, agent: { kind: "main", id: "Main", name: "Main", depth: 0 },
		isIdle: () => true, hasPendingMessages: () => false, ui: { notify: (message: string) => { notices.push(message); } } } satisfies Parameters<ReturnType<typeof createTeamsBridge>["start"]>[0];
	const pi = { zod, sendMessage: (message: Parameters<ExtensionAPI["sendMessage"]>[0]) => { sent.push(message); } };
	const call: Control = async request => {
		controls.push(request);
		switch (request.name) {
			case "teams_bind_session": return { bound: true, session_id: manager.getSessionId(), binding_id: "binding-1" };
			case "teams_check_session": return { state: "active" };
			case "teams_recover_session": return { state: "active", ...request.arguments };
			case "teams_unbind_session": return { bound: false };
			case "teams_ack_event": return { acknowledged: true };
		}
	};
	const bridge = createTeamsBridge(pi, call, fileStorage);
	const event = { version: 1, content: "synthetic Teams message", meta: { provider: "teams", tenant_id: "tenant", bot_id: "bot",
		conversation_id: "conversation", message_id: "activity", channel_id: "msteams", user_id: "sender",
		timestamp: "2026-10-13T10:00:00Z", reply_handle: "opaque-handle", mcp_session_id: manager.getSessionId(),
		binding_id: "binding-1", event_id: "9007199254740993", delivery_token: "token-1" } };
	const notice = { server: "dione", method: "notifications/dione/event", params: event };
	return { manager, ctx, bridge, event, notice, controls, sent, notices, call, pi };
}

test("real saved OMP transcript gates binding and exact durable receipt gates ACK", async () => {
	const f = await fixture();
	try {
		await f.bridge.start(f.ctx);
		expect(f.controls.map(request => request.name)).toEqual(["teams_bind_session"]);
		f.bridge.receive(f.notice);
		await f.bridge.pump();
		expect(f.controls.filter(request => request.name === "teams_ack_event")).toEqual([]);
		expect(f.sent).toHaveLength(1);
		const [message] = f.sent;
		if (!message || typeof message !== "object" || Array.isArray(message)) throw new Error("expected object submission");
		expect(JSON.stringify(message)).not.toContain("delivery_token");
		expect(JSON.stringify(message)).not.toContain("binding_id");
		f.manager.appendCustomMessageEntry(message.customType, message.content, message.display, message.details, "agent");
		await f.manager.flush();
		await f.bridge.pump();
		expect(f.controls.filter(request => request.name === "teams_ack_event")).toEqual([
			{ name: "teams_ack_event", arguments: { binding_id: "binding-1", delivery_token: "token-1" } },
		]);
		expect(f.bridge.beforeTransition()).toEqual({ cancel: true });
		await f.bridge.stop();
		expect(f.bridge.beforeTransition()).toBeUndefined();
	} finally { await f.manager.close(); }
});

test("silent void submission remains visible, vetoes transition, and renewed leases never resubmit", async () => {
	const f = await fixture();
	try {
		await f.bridge.start(f.ctx);
		f.bridge.receive(f.notice);
		await f.bridge.pump();
		f.bridge.receive({ ...f.notice, params: { ...f.event, meta: { ...f.event.meta, delivery_token: "token-2" } } });
		await f.bridge.pump();
		expect(f.sent).toHaveLength(1);
		expect(f.controls.filter(request => request.name === "teams_ack_event")).toEqual([]);
		expect(await f.bridge.beforeTransition()).toEqual({ cancel: true });
		expect(f.notices.some(message => message.includes("awaits a saved transcript receipt"))).toBe(true);
	} finally { await f.bridge.stop(); await f.manager.close(); }
});

test("lazy file and unsupported backend remain unbound until saved file becomes available", async () => {
	const f = await fixture(false);
	const unsupported = await fixture(true, false);
	try {
		await f.bridge.start(f.ctx);
		expect(f.controls).toEqual([]);
		await f.manager.ensureOnDisk();
		await f.bridge.pump();
		expect(f.controls.map(request => request.name)).toEqual(["teams_bind_session"]);
		await unsupported.bridge.start(unsupported.ctx);
		expect(unsupported.controls).toEqual([]);
		expect(unsupported.notices[0]).toContain("FileSessionStorage");
	} finally { await f.bridge.stop(); await f.manager.close(); await unsupported.manager.close(); }
});

test("wrong session, binding, precision-losing event ID and malformed Teams frames never submit", async () => {
	const f = await fixture();
	try {
		await f.bridge.start(f.ctx);
		for (const patch of [{ mcp_session_id: "other" }, { binding_id: "old" }, { event_id: 3 }, { message_id: "bad\nidentity" }]) {
			f.bridge.receive({ ...f.notice, params: { ...f.event, meta: { ...f.event.meta, ...patch } } });
		}
		await f.bridge.pump();
		expect(f.sent).toEqual([]);
		f.bridge.receive(f.notice);
		await f.bridge.pump();
		expect(f.sent).toHaveLength(1);
	} finally { await f.bridge.stop(); await f.manager.close(); }
});

test("receipt requires exact native tuple and SDK agreement; incomplete transcript never ACKs", async () => {
	const f = await fixture();
	try {
		const schemas = teamsSchemas(zod);
		const event = schemas.event.parse(f.event);
		const receipt = receiptFor(event);
		f.manager.appendCustomMessageEntry(TEAMS_RECEIPT, "synthetic", true, receipt, "agent");
		await f.manager.flush();
		const file = f.manager.getSessionFile();
		if (!file) throw new Error("missing session file");
		const session = { id: f.manager.getSessionId(), file, cwd: f.manager.getRecordedCwd() ?? "" };
		expect(await transcriptReceipts(session, f.ctx, schemas)).toEqual([receipt]);
		const text = await readFile(file, "utf8");
		await writeFile(file, text.replace('"conversation_id":"conversation"', '"conversation_id":"another"'));
		await expect(transcriptReceipts(session, f.ctx, schemas)).rejects.toThrow("differs from host");
		await writeFile(file, text.trimEnd());
		await expect(transcriptReceipts(session, f.ctx, schemas)).rejects.toThrow("incomplete");
		await writeFile(file, text);
	} finally { await f.manager.close(); }
});

test("shutdown drains a late bind response and revokes its capability without submission", async () => {
	const f = await fixture();
	let entered: () => void = () => {};
	let release: () => void = () => {};
	const called = new Promise<void>(resolve => { entered = resolve; });
	const barrier = new Promise<void>(resolve => { release = resolve; });
	const bridge = createTeamsBridge(f.pi, async request => {
		if (request.name === "teams_bind_session") { entered(); await barrier; }
		return f.call(request);
	}, true);
	try {
		const start = bridge.start(f.ctx);
		await called;
		const stop = bridge.stop();
		release();
		await Promise.all([start, stop]);
		bridge.receive(f.notice);
		await bridge.pump();
		expect(f.controls.map(request => request.name)).toEqual(["teams_bind_session", "teams_unbind_session"]);
		expect(f.sent).toEqual([]);
	} finally { await f.manager.close(); }
});

test("disk receipt deduplicates replay after extension restart", async () => {
	const f = await fixture();
	try {
		const event = teamsSchemas(zod).event.parse(f.event);
		f.manager.appendCustomMessageEntry(TEAMS_RECEIPT, "already submitted", true, receiptFor(event), "agent");
		await f.manager.flush();
		await f.bridge.start(f.ctx);
		f.bridge.receive(f.notice);
		await f.bridge.pump();
		expect(f.sent).toEqual([]);
		expect(f.controls.map(request => request.name)).toEqual(["teams_bind_session", "teams_check_session", "teams_ack_event"]);
	} finally { await f.bridge.stop(); await f.manager.close(); }
});

test("saved receipt with the same key and session but another native tuple cannot settle submission", async () => {
	const f = await fixture();
	try {
		await f.bridge.start(f.ctx);
		f.bridge.receive(f.notice);
		await f.bridge.pump();
		const receipt = receiptFor(teamsSchemas(zod).event.parse(f.event));
		const different = { ...receipt, native: { ...receipt.native, conversation_id: "another" } };
		// Both disk and SDK contain this schema-valid receipt, not the submitted tuple.
		f.manager.appendCustomMessageEntry(TEAMS_RECEIPT, "synthetic mismatching receipt", true, different, "agent");
		await f.manager.flush();
		const file = f.manager.getSessionFile();
		if (!file) throw new Error("missing session file");
		expect(await transcriptReceipts({ id: f.manager.getSessionId(), file, cwd: f.manager.getRecordedCwd() ?? "" }, f.ctx, teamsSchemas(zod)))
			.toEqual([different]);
		await f.bridge.pump();
		expect(f.sent).toHaveLength(1);
		expect(f.controls.filter(request => request.name === "teams_ack_event")).toEqual([]);
		await f.bridge.stop();
		expect(f.bridge.beforeTransition()).toEqual({ cancel: true });
	} finally { await f.bridge.stop(); await f.manager.close(); }
});

test("committed ACK response loss settles saved submission without replay or resubmission", async () => {
	const f = await fixture();
	let acknowledged = false;
	let generation = 0;
	const recoveries: Parameters<Control>[0][] = [];
	const bridge = createTeamsBridge(f.pi, async request => {
		if (request.name === "teams_bind_session") return {
			bound: true, session_id: f.manager.getSessionId(), binding_id: `binding-${++generation}`,
		};
		if (request.name === "teams_recover_session") {
			recoveries.push(request);
			expect(request.arguments).toEqual({ session_id: f.manager.getSessionId(), binding_id: "binding-1" });
			return { state: "active", ...request.arguments };
		}
		if (request.name === "teams_ack_event") {
			acknowledged = true;
			throw new Error("ACK committed but response lost");
		}
		return f.call(request);
	}, true);
	try {
		await bridge.start(f.ctx);
		bridge.receive(f.notice);
		await bridge.pump();
		expect(bridge.beforeTransition()).toEqual({ cancel: true });
		const [message] = f.sent;
		if (!message || typeof message !== "object" || Array.isArray(message)) throw new Error("expected object submission");
		f.manager.appendCustomMessageEntry(message.customType, message.content, message.display, message.details, "agent");
		await f.manager.flush();
		await bridge.pump();
		expect(acknowledged).toBe(true);
		// The server consumed the event: healthy recovery has nothing to replay.
		await bridge.pump();
		expect(generation).toBe(1);
		expect(recoveries).toEqual([{ name: "teams_recover_session", arguments: { session_id: f.manager.getSessionId(), binding_id: "binding-1" } }]);
		expect(f.sent).toHaveLength(1);
		await bridge.stop();
		expect(bridge.beforeTransition()).toBeUndefined();
	} finally { await bridge.stop(); await f.manager.close(); }
});

test("lost bind response stays uncertain until exact-session recovery and revocation succeed", async () => {
	const f = await fixture();
	let bound = false;
	let loseResponse = true;
	const bridge = createTeamsBridge(f.pi, async request => {
		if (request.name === "teams_bind_session") {
			bound = true;
			if (loseResponse) throw new Error("server committed; response lost");
		}
		if (request.name === "teams_unbind_session") bound = false;
		return f.call(request);
	}, true);
	try {
		await bridge.start(f.ctx);
		expect(bound).toBe(true);
		expect(bridge.beforeTransition()).toEqual({ cancel: true });
		await expect(bridge.stop()).rejects.toThrow("response lost");
		expect(bound).toBe(true);
		expect(bridge.beforeTransition()).toEqual({ cancel: true });
		await expect(bridge.stop()).rejects.toThrow("response lost");
		loseResponse = false;
		expect(bridge.beforeTransition()).toEqual({ cancel: true });
		await bridge.stop();
		expect(bound).toBe(false);
		expect(bridge.beforeTransition()).toBeUndefined();
		expect(f.sent).toEqual([]);
	} finally { await f.manager.close(); }
});

test("fenced stale proof retires own capability after a lost successful unbind response", async () => {
	for (const loseInitialBind of [true, false]) {
		const f = await fixture();
		let sequence = 0;
		let active: string | undefined;
		let loseUnbind = true;
		const revoked: string[] = [];
		const bridge = createTeamsBridge(f.pi, async request => {
			if (request.name === "teams_bind_session") {
				active = `binding-${++sequence}`;
				if (sequence === 1 && loseInitialBind) throw new Error("lost initial bind response");
				return { bound: true, session_id: f.manager.getSessionId(), binding_id: active };
			}
			if (request.name === "teams_recover_session") {
				expect(request.arguments.session_id).toBe(f.manager.getSessionId());
				expect(request.arguments.binding_id).toBe(`binding-${sequence}`);
				return active === request.arguments.binding_id ? { state: "active", ...request.arguments } : { state: "stale" };
			}
			if (request.name === "teams_unbind_session") {
				if (request.arguments.binding_id !== active) throw new Error("stale binding");
				revoked.push(active);
				active = undefined;
				if (loseUnbind) { loseUnbind = false; throw new Error("lost successful unbind response"); }
				return { bound: false };
			}
			return { acknowledged: false };
		}, true);
		try {
			await bridge.start(f.ctx);
			await expect(bridge.stop()).rejects.toThrow("lost successful unbind response");
			expect(active).toBeUndefined();
			expect(bridge.beforeTransition()).toEqual({ cancel: true });
			await bridge.stop();
			expect(active).toBeUndefined();
			expect(revoked).toEqual(loseInitialBind ? ["binding-2"] : ["binding-1"]);
			expect(sequence).toBe(loseInitialBind ? 2 : 1);
			expect(bridge.beforeTransition()).toBeUndefined();
			await bridge.stop();
			expect(bridge.beforeTransition()).toBeUndefined();
		} finally { await f.manager.close(); }
	}
});

test("actual OMP before hook cancels promptly across its 30-second budget while bind drains", async () => {
	const f = await fixture();
	let entered: () => void = () => {};
	let release: () => void = () => {};
	const called = new Promise<void>(resolve => { entered = resolve; });
	const barrier = new Promise<void>(resolve => { release = resolve; });
	const bridge = createTeamsBridge(f.pi, async request => {
		if (request.name === "teams_bind_session") { entered(); await barrier; }
		return f.call(request);
	}, true);
	const root = await mkdtemp(join(tmpdir(), "dione-teams-hook-"));
	const auth = await AuthStorage.create(join(root, "empty-auth.db"));
	const models = new ModelRegistry(auth, join(root, "models.json"));
	const loaded = await loadExtensions([join(import.meta.dir, "dione.ts")], root);
	const [extension] = loaded.extensions;
	if (!extension) throw new Error("extension did not load");
	extension.handlers.set("session_before_switch", [async () => bridge.beforeTransition()]);
	const runner = new ExtensionRunner(loaded.extensions, loaded.runtime, root, f.manager, models);
	const errors: string[] = [];
	runner.onError(error => errors.push(error.error));
	const start = bridge.start(f.ctx);
	try {
		await called;
		expect(await runner.emit({ type: "session_before_switch", reason: "new" })).toEqual({ cancel: true });
		await new Promise<void>(resolve => setTimeout(resolve, 31_000));
		expect(await runner.emit({ type: "session_before_switch", reason: "new" })).toEqual({ cancel: true });
		expect(errors).toEqual([]);
		release();
		await start;
		await bridge.stop();
		expect(await runner.emit({ type: "session_before_switch", reason: "new" })).toBeUndefined();
		expect(f.controls.map(request => request.name)).toEqual(["teams_bind_session", "teams_unbind_session"]);
	} finally { release(); await start; await bridge.stop(); await f.manager.close(); await auth.close(); }
}, 40_000);

test("Teams-enabled actual loader owns delivery, saved ACK, transition veto and shutdown through a private Unix peer", async () => {
	const root = await mkdtemp(join(tmpdir(), "dione-teams-native-"));
	// Retain this synthetic fixture even on failure for allowlisted evidence capture.
	console.info(`Synthetic Teams lifecycle fixture retained: ${root}`);
	await chmod(root, 0o700);
	const socketPath = join(root, "control.sock");
	const manager = SessionManager.create(root, join(root, "sessions"));
	await manager.ensureOnDisk();
	const auth = await AuthStorage.create(join(root, "empty-auth.db"));
	const models = new ModelRegistry(auth, join(root, "models.json"));
	const requests: { name: string; arguments: Record<string, string> }[] = [];
	const sent: Parameters<ExtensionAPI["sendMessage"]>[0][] = [];
	const errors: string[] = [];
	let active = false;
	const server = createServer(socket => {
		let input = "";
		socket.setEncoding("utf8");
		socket.on("data", chunk => {
			input += chunk;
			if (!input.includes("\n")) return;
			try {
				const frame = JSON.parse(input.slice(0, input.indexOf("\n")));
				const request = frame.params;
				requests.push(request);
				let result: unknown;
				if (request.name === "teams_bind_session" && request.arguments.session_id === manager.getSessionId()) {
					active = true;
					result = { bound: true, session_id: manager.getSessionId(), binding_id: "native-binding" };
				} else if (request.name === "teams_check_session" && active
					&& request.arguments.session_id === manager.getSessionId() && request.arguments.binding_id === "native-binding") {
					result = { state: "active" };
				} else if (request.name === "teams_ack_event" && active
					&& request.arguments.binding_id === "native-binding" && request.arguments.delivery_token === "native-token") {
					result = { acknowledged: true };
				} else if (request.name === "teams_unbind_session" && request.arguments.binding_id === "native-binding") {
					active = false;
					result = { bound: false };
				} else throw new Error("unexpected private control request");
				socket.end(JSON.stringify({ jsonrpc: "2.0", id: frame.id,
					result: { content: [{ type: "text", text: JSON.stringify(result) }] } }) + "\n");
			} catch (error) { socket.destroy(error instanceof Error ? error : new Error("invalid fake peer request")); }
		});
		socket.on("error", error => errors.push(error.message));
	});
	await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(socketPath, resolve); });
	await chmod(socketPath, 0o600);
	const previousSocket = process.env.DIONE_TEAMS_CONTROL_SOCKET;
	const previousStorage = process.env.DIONE_TEAMS_SESSION_STORAGE;
	process.env.DIONE_TEAMS_CONTROL_SOCKET = socketPath;
	process.env.DIONE_TEAMS_SESSION_STORAGE = "file";
	let runner: ExtensionRunner | undefined;
	async function observed(predicate: () => boolean): Promise<void> {
		const deadline = Date.now() + 5000;
		while (!predicate()) {
			if (Date.now() >= deadline) throw new Error("native Teams lifecycle effect was not observed");
			await new Promise(resolve => setTimeout(resolve, 10));
		}
	}
	try {
		const loaded = await loadExtensions([join(import.meta.dir, "dione.ts")], root);
		expect(loaded.errors).toEqual([]);
		runner = new ExtensionRunner(loaded.extensions, loaded.runtime, root, manager, models);
		runner.onError(error => errors.push(error.error));
		runner.initialize({
			sendMessage: message => { sent.push(message); },
			sendUserMessage: () => { throw new Error("unexpected user message"); },
			appendEntry: () => {}, setLabel: () => {}, getActiveTools: () => [],
			getAllTools: () => [], setActiveTools: async () => {}, getCommands: () => [],
			setModel: async () => false, getThinkingLevel: () => undefined,
			setThinkingLevel: () => {}, getSessionName: () => undefined, setSessionName: async () => {},
		}, {
			getModel: () => undefined, isIdle: () => true, abort: () => {},
			hasPendingMessages: () => false, shutdown: () => {}, getContextUsage: () => undefined,
			compact: async () => {}, getSystemPrompt: () => [],
		}, undefined, undefined, "tui");
		await runner.emit({ type: "session_start" });
		expect(requests.map(request => request.name)).toEqual(["teams_bind_session"]);
		await runner.emitMcpNotification({ server: "dione", method: "notifications/dione/event", params: {
			version: 1, content: "native synthetic Teams event", meta: { provider: "teams", tenant_id: "tenant", bot_id: "bot",
				conversation_id: "conversation", message_id: "native-activity", channel_id: "msteams", user_id: "sender",
				timestamp: "2026-10-18T00:00:00Z", reply_handle: "opaque-native-handle", mcp_session_id: manager.getSessionId(),
				binding_id: "native-binding", event_id: "1", delivery_token: "native-token" },
		} });
		await observed(() => sent.length === 1);
		expect(requests.filter(request => request.name === "teams_ack_event")).toHaveLength(0);
		expect(await runner.emit({ type: "session_before_switch", reason: "new" })).toEqual({ cancel: true });
		const [message] = sent;
		if (!message || typeof message !== "object" || Array.isArray(message)) throw new Error("expected native object submission");
		expect(message.customType).toBe(TEAMS_RECEIPT);
		expect(message.content).toContain("native synthetic Teams event");
		manager.appendCustomMessageEntry(message.customType, message.content, message.display, message.details, "agent");
		await manager.flush();
		await observed(() => requests.some(request => request.name === "teams_ack_event"));
		await runner.emit({ type: "session_shutdown" });
		expect(active).toBe(false);
		expect(requests.filter(request => request.name !== "teams_check_session").map(request => request.name))
			.toEqual(["teams_bind_session", "teams_ack_event", "teams_unbind_session"]);
		const checks = requests.filter(request => request.name === "teams_check_session");
		expect(checks.length).toBeGreaterThan(0);
		for (const check of checks) expect(check.arguments).toEqual({ session_id: manager.getSessionId(), binding_id: "native-binding" });
		expect(await runner.emit({ type: "session_before_switch", reason: "new" })).toBeUndefined();
		expect(sent).toHaveLength(1);
		expect(errors).toEqual([]);
	} finally {
		if (runner) await runner.emit({ type: "session_shutdown" });
		if (previousSocket === undefined) delete process.env.DIONE_TEAMS_CONTROL_SOCKET;
		else process.env.DIONE_TEAMS_CONTROL_SOCKET = previousSocket;
		if (previousStorage === undefined) delete process.env.DIONE_TEAMS_SESSION_STORAGE;
		else process.env.DIONE_TEAMS_SESSION_STORAGE = previousStorage;
		await manager.close();
		await auth.close();
		await new Promise<void>(resolve => server.close(() => resolve()));
	}
}, 20_000);


test("empty pending pump recovers only the exact uncertainty predecessor and resumes delivery", async () => {
	const f = await fixture();
	let inactive = false;
	let failRecovery = false;
	const calls: Parameters<Control>[0][] = [];
	const bridge = createTeamsBridge(f.pi, async request => {
		calls.push(request);
		if (request.name === "teams_check_session") return { state: inactive ? "inactive" : "active" };
		if (request.name === "teams_recover_session") {
			expect(request.arguments).toEqual({ session_id: f.manager.getSessionId(), binding_id: "binding-1" });
			if (failRecovery) throw new Error("persistent durability barrier failure");
			inactive = false;
			return { state: "active", session_id: f.manager.getSessionId(), binding_id: "binding-2" };
		}
		return f.call(request);
	}, true);
	try {
		await bridge.start(f.ctx);
		inactive = true;
		failRecovery = true;
		await bridge.pump();
		await bridge.pump();
		expect(f.sent).toHaveLength(0);
		expect(calls.filter(call => call.name === "teams_bind_session")).toHaveLength(1);
		failRecovery = false;
		await bridge.pump();
		bridge.receive({ ...f.notice, params: { ...f.event, meta: { ...f.event.meta, binding_id: "binding-2" } } });
		await bridge.pump();
		expect(f.sent).toHaveLength(1);
		expect(bridge.beforeTransition()).toEqual({ cancel: true });
	} finally { await bridge.stop(); await f.manager.close(); }
});

test("stale same-session or foreign controller never invokes ordinary bind recovery", async () => {
	for (const staleAt of ["check", "recover"] as const) {
		const f = await fixture();
		const calls: Parameters<Control>[0][] = [];
		const bridge = createTeamsBridge(f.pi, async request => {
			calls.push(request);
			if (request.name === "teams_check_session") return { state: staleAt === "check" ? "stale" : "inactive" };
			if (request.name === "teams_recover_session") return { state: "stale" };
			return f.call(request);
		}, true);
		try {
			await bridge.start(f.ctx);
			await bridge.pump();
			await bridge.pump();
			await bridge.stop();
			expect(calls.filter(call => call.name === "teams_bind_session")).toHaveLength(1);
			expect(calls.filter(call => call.name === "teams_unbind_session")).toHaveLength(0);
			expect(f.sent).toHaveLength(0);
		} finally { await bridge.stop(); await f.manager.close(); }
	}
});

test("stop drains a late fenced recovery and revokes its exact active result", async () => {
	const f = await fixture();
	let entered!: () => void;
	let release!: () => void;
	const started = new Promise<void>(resolve => { entered = resolve; });
	const barrier = new Promise<void>(resolve => { release = resolve; });
	const calls: Parameters<Control>[0][] = [];
	const bridge = createTeamsBridge(f.pi, async request => {
		calls.push(request);
		if (request.name === "teams_check_session") return { state: "inactive" };
		if (request.name === "teams_recover_session") {
			entered(); await barrier;
			return { state: "active", session_id: f.manager.getSessionId(), binding_id: "binding-2" };
		}
		return f.call(request);
	}, true);
	try {
		await bridge.start(f.ctx);
		const pumping = bridge.pump();
		await started;
		const stopping = bridge.stop();
		expect(bridge.beforeTransition()).toEqual({ cancel: true });
		release();
		await pumping;
		await stopping;
		expect(calls.filter(call => call.name === "teams_unbind_session")).toEqual([
			{ name: "teams_unbind_session", arguments: { binding_id: "binding-2" } },
		]);
		expect(calls.filter(call => call.name === "teams_bind_session")).toHaveLength(1);
		expect(bridge.beforeTransition()).toBeUndefined();
	} finally { release(); await bridge.stop(); await f.manager.close(); }
});

test("unknown recovery response stays fenced until same-predecessor retry and confirmed stop", async () => {
	const f = await fixture();
	let lose = true;
	let inactive = true;
	const calls: Parameters<Control>[0][] = [];
	const bridge = createTeamsBridge(f.pi, async request => {
		calls.push(request);
		if (request.name === "teams_check_session") return { state: inactive ? "inactive" : "active" };
		if (request.name === "teams_recover_session") {
			expect(request.arguments).toEqual({ session_id: f.manager.getSessionId(), binding_id: "binding-1" });
			inactive = false;
			if (lose) throw new Error("recovery committed but response lost");
			return { state: "active", session_id: f.manager.getSessionId(), binding_id: "binding-2" };
		}
		return f.call(request);
	}, true);
	try {
		await bridge.start(f.ctx);
		await bridge.pump();
		await expect(bridge.stop()).rejects.toThrow("recovery committed but response lost");
		expect(bridge.beforeTransition()).toEqual({ cancel: true });
		lose = false;
		await bridge.stop();
		expect(calls.filter(call => call.name === "teams_unbind_session")).toEqual([
			{ name: "teams_unbind_session", arguments: { binding_id: "binding-2" } },
		]);
		expect(calls.filter(call => call.name === "teams_bind_session")).toHaveLength(1);
		expect(f.sent).toHaveLength(0);
		expect(bridge.beforeTransition()).toBeUndefined();
	} finally { lose = false; await bridge.stop(); await f.manager.close(); }
});

test("stop invalidates the exact still-inactive recovery fence", async () => {
	const f = await fixture();
	let disconnected = false;
	const calls: Parameters<Control>[0][] = [];
	const bridge = createTeamsBridge(f.pi, async request => {
		calls.push(request);
		if (request.name === "teams_check_session") return { state: "inactive" };
		if (request.name === "teams_recover_session") return disconnected ? { state: "stale" }
			: { state: "inactive", session_id: f.manager.getSessionId(), binding_id: "binding-2" };
		if (request.name === "teams_unbind_session") disconnected = true;
		return f.call(request);
	}, true);
	try {
		await bridge.start(f.ctx);
		await bridge.pump();
		await bridge.stop();
		expect(calls.filter(call => call.name === "teams_unbind_session")).toEqual([
			{ name: "teams_unbind_session", arguments: { binding_id: "binding-2" } },
		]);
		expect(disconnected).toBe(true);
		expect(bridge.beforeTransition()).toBeUndefined();
	} finally { await bridge.stop(); await f.manager.close(); }
});

test("late inactive recovery remains owned through stop and lost cleanup response", async () => {
	for (const loseCleanup of [false, true]) {
		const f = await fixture();
		let entered!: () => void;
		let release!: () => void;
		const started = new Promise<void>(resolve => { entered = resolve; });
		const barrier = new Promise<void>(resolve => { release = resolve; });
		let fence = true;
		const calls: Parameters<Control>[0][] = [];
		const bridge = createTeamsBridge(f.pi, async request => {
			calls.push(request);
			if (request.name === "teams_check_session") return { state: "inactive" };
			if (request.name === "teams_recover_session") {
				expect(request.arguments.session_id).toBe(f.manager.getSessionId());
				if (request.arguments.binding_id === "binding-1") {
					entered(); await barrier;
					return { state: "inactive", session_id: f.manager.getSessionId(), binding_id: "binding-2" };
				}
				expect(request.arguments.binding_id).toBe("binding-2");
				expect(fence).toBe(false);
				return { state: "stale" };
			}
			if (request.name === "teams_unbind_session") {
				expect(request.arguments.binding_id).toBe("binding-2");
				fence = false;
				if (loseCleanup) throw new Error("inactive cleanup committed but response lost");
			}
			return f.call(request);
		}, true);
		try {
			await bridge.start(f.ctx);
			const pumping = bridge.pump();
			await started;
			const stopping = bridge.stop();
			expect(bridge.beforeTransition()).toEqual({ cancel: true });
			expect(fence).toBe(true);
			release();
			await pumping;
			if (loseCleanup) {
				await expect(stopping).rejects.toThrow("inactive cleanup committed but response lost");
				expect(bridge.beforeTransition()).toEqual({ cancel: true });
				await bridge.stop();
				expect(calls.filter(call => call.name === "teams_recover_session").at(-1)).toEqual({
					name: "teams_recover_session", arguments: { session_id: f.manager.getSessionId(), binding_id: "binding-2" },
				});
			} else await stopping;
			expect(fence).toBe(false);
			expect(calls.filter(call => call.name === "teams_unbind_session")).toEqual([
				{ name: "teams_unbind_session", arguments: { binding_id: "binding-2" } },
			]);
			expect(calls.filter(call => call.name === "teams_bind_session")).toHaveLength(1);
			expect(f.sent).toHaveLength(0);
			expect(bridge.beforeTransition()).toBeUndefined();
		} finally { release(); await bridge.stop(); await f.manager.close(); }
	}
});
