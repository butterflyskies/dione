#!/usr/bin/env python3
"""Scan every publishable workspace archive using Cargo's resolved membership."""
import json
from pathlib import Path
import subprocess
import sys


def main():
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        stdout=subprocess.PIPE, text=True,
    )
    if metadata.returncode:
        return metadata.returncode
    workspace = json.loads(metadata.stdout)
    members = set(workspace["workspace_members"])
    scanner = Path(__file__).resolve().with_name("verify-public-package-privacy.sh")
    for package in workspace["packages"]:
        if package["id"] not in members or package.get("publish") == []:
            continue
        archive = Path(workspace["target_directory"]) / "package" / (package["name"] + "-" + package["version"] + ".crate")
        result = subprocess.run(["sh", str(scanner), str(archive)])
        if result.returncode:
            return result.returncode
    return 0


if __name__ == "__main__":
    sys.exit(main())
