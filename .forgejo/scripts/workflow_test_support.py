"""Strict readers for the workflow subset exercised by release tests."""
import re


def read_jobs(workflow):
    jobs = {}
    current = None
    in_jobs = False
    for line in workflow.splitlines():
        if line == "jobs:":
            in_jobs = True
            continue
        if not in_jobs:
            continue
        if line and not line.startswith(" "):
            break
        match = re.fullmatch(r"  ([\w-]+):", line)
        if match:
            current = match.group(1)
            if current in jobs:
                raise ValueError("duplicate workflow job: " + current)
            jobs[current] = ""
        elif current:
            jobs[current] += line + "\n"
    return jobs


def run_blocks(job):
    lines = job.splitlines()
    blocks = []
    for index, line in enumerate(lines):
        if line.strip() != "run: |":
            continue
        indent = len(line) - len(line.lstrip()) + 2
        block = []
        for following in lines[index + 1:]:
            if following.strip() and len(following) - len(following.lstrip()) < indent:
                break
            block.append(following[indent:])
        blocks.append("\n".join(block))
    return blocks
