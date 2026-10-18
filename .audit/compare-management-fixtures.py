from pathlib import Path
import re
import subprocess
import sys

baseline = sys.argv[1]
root = Path(sys.argv[2]) if len(sys.argv) > 2 else Path.cwd()

def balanced(text, start, opening, closing):
    depth, quoted, escaped = 0, False, False
    for end in range(start, len(text)):
        char = text[end]
        if escaped:
            escaped = False
        elif quoted and char == chr(92):
            escaped = True
        elif char == '"':
            quoted = not quoted
        elif not quoted:
            depth += (char == opening) - (char == closing)
            if depth == 0:
                return text[start:end + 1]
    raise AssertionError('Unclosed expression')

def compact(text):
    return ''.join(re.findall(r'"(?:\\.|[^"\\])*"|\S', text))

def fixtures(sources):
    found = {}
    for text in sources:
        for match in re.finditer(r'fn (management_contract_\w+)\(', text):
            body = balanced(text, text.index('{', match.end()), '{', '}')
            calls, offset = [], 0
            while (start := body.find('json!(', offset)) >= 0:
                call = balanced(body, start + 5, '(', ')')
                calls.append(compact(call))
                offset = start + 5 + len(call)
            assert calls and match.group(1) not in found, match.group(1)
            found[match.group(1)] = calls
    return found

old = fixtures(subprocess.check_output(['git', '-C', str(root), 'show', f'{baseline}:{path}']).decode('utf-8') for path in ['src/mcp/tools/management.rs', 'src/mcp/dispatch.rs'])
new = fixtures(path.read_text(encoding='utf-8-sig') for path in (root / 'src').rglob('*.rs'))
assert old and old == new, f'Changed fixtures: {old.keys() ^ new.keys() or [name for name in old if old[name] != new[name]]}'
print(f'{len(old)} tests retain {sum(map(len, old.values()))} JSON expressions from committed baseline {baseline}, ignoring formatting whitespace only.')
