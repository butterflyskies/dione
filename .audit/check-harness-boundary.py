"""Reject concrete harness dependencies in shared production boundaries."""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
FILES = (
    "src/mcp/server.rs", "src/mcp/dispatch.rs", "src/mcp/protocol.rs",
    "src/mcp/harness_wire.rs", "src/attention/runtime.rs", "src/attention/delivery.rs",
)
LITERALS = re.compile(r'r(?P<hashes>\#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"|//[^\n]*|/\*.*?\*/|\'(?:\\.|[^\'\\])\'', re.S)
FORBIDDEN = re.compile(r"\b(?:codex|TransportMode|NotificationSink|codex_queue|codex_thread_binding)\b")
MAIN_FORBIDDEN = re.compile(r"\b(?:CodexEventQueue|CodexDeliveryConfig|run_delivery_worker|binding_tx|binding_rx|delivery_cancel|codex_handle)\b|\bwatch\s*::")
TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def production(text: str) -> str:
    masked = LITERALS.sub(lambda match: " " * len(match.group()), text)
    while match := TEST_ATTRIBUTE.search(masked):
        start = match.start()
        cursor = match.end()
        while cursor < len(masked):
            if masked[cursor] == ";":
                cursor += 1
                break
            if masked[cursor] == "{":
                depth = 1
                cursor += 1
                while depth:
                    if cursor == len(masked):
                        raise ValueError("unclosed test item")
                    depth += (masked[cursor] == "{") - (masked[cursor] == "}")
                    cursor += 1
                break
            cursor += 1
        masked = masked[:start] + " " * (cursor - start) + masked[cursor:]
    return masked


def check(text: str) -> list[str]:
    return [match.group() for match in FORBIDDEN.finditer(production(text))]


def main() -> None:
    assert check('fn route() { crate::codex::Queue; }') == ['codex']
    assert check('#[cfg(test)] mod tests { fn x() { let _ = "}"; crate::codex::Queue; } } fn route() {}') == []
    assert check('#[cfg(test)] #[path="fixture.rs"] mod fixture; fn route() { TransportMode; }') == ['TransportMode']
    assert check('fn route() { let _ = "codex::Queue"; } // TransportMode') == []
    assert check('use crate::codex::{CodexEventQueue};') == ['codex']
    assert check('use crate::{codex, mcp};') == ['codex']
    assert check('use crate::codex:: {CodexEventQueue};') == ['codex']
    errors = [(name, check((ROOT / name).read_text(encoding="utf-8"))) for name in FILES]
    failures = [f"{name}: {', '.join(found)}" for name, found in errors if found]
    assert MAIN_FORBIDDEN.search(production('use dione::codex::CodexEventQueue;'))
    assert MAIN_FORBIDDEN.search(production('let (tx, rx) = watch::channel(None);'))
    assert not MAIN_FORBIDDEN.search(production('let thread: dione::codex::CodexThreadId;'))
    for name in ("src/main.rs", "src/startup.rs"):
        found = MAIN_FORBIDDEN.findall(production((ROOT / name).read_text(encoding="utf-8")))
        if found:
            failures.append(f"{name}: {', '.join(found)}")
    if failures:
        raise SystemExit("\n".join(failures))
    print(f"PASS: {len(FILES)} shared production boundaries have no concrete Codex routing dependency")
    print("PASS: main and startup have no concrete queue, binding channel, or delivery worker ownership names")


if __name__ == "__main__":
    main()
