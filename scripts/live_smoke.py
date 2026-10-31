#!/usr/bin/env python3
"""Exercise the real CLI against a disposable set of notes on a Memos instance."""

import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen
import uuid


def settings(path):
    values = {}
    if path.exists():
        for line in path.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, separator, value = line.removeprefix("export ").partition("=")
            if separator:
                parts = shlex.split(value, comments=True)
                values[key.strip()] = " ".join(parts)
    values.update(os.environ)
    return values


def unpack(path):
    text = path.read_text()
    assert text.startswith("---\n"), "Missing sync frontmatter"
    header, body = text[4:].split("\n---\n", 1)
    for line in header.splitlines():
        if line.startswith("memos_id:"):
            return line.partition(":")[2].strip().strip("\"'"), body
    raise AssertionError("Missing memo identity")


def edit(path, body):
    header, _ = path.read_text().split("\n---\n", 1)
    path.write_text(header + "\n---\n" + body)


def main():
    project = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-remote-writes", action="store_true",
                        help="Required: create, edit, and delete disposable test memos")
    parser.add_argument("--env-file", type=Path, default=project / ".env")
    parser.add_argument("--binary", type=Path, default=project / "target/debug/memos")
    args = parser.parse_args()
    if not args.allow_remote_writes:
        parser.error("Pass --allow-remote-writes to authorize the live test")
    config = settings(args.env_file)
    token = config.get("TOKEN")
    root = config.get("MEMOS_URL", "").rstrip("/")
    if not token or not root:
        parser.error("MEMOS_URL and TOKEN must be configured")
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error("Build the CLI first with cargo build, or specify --binary")

    tracked = set()
    prefix = "memos-sync-live-" + uuid.uuid4().hex[:10]

    def api(method, route, body=None, query=None, allow_missing=False):
        url = root + "/api/v1/" + route
        if query:
            url += "?" + urlencode(query)
        request = Request(
            url,
            data=None if body is None else json.dumps(body).encode(),
            method=method,
            headers={"Authorization": "Bearer " + token,
                     "Accept": "application/json", "Content-Type": "application/json"},
        )
        try:
            with urlopen(request, timeout=15) as response:
                payload = response.read()
                return json.loads(payload) if payload else None
        except HTTPError as error:
            if allow_missing and error.code == 404:
                return None
            raise RuntimeError(f"Live API {method} failed with HTTP {error.code}") from None

    def create(label):
        memo_id = str(uuid.uuid4())
        name = "memos/" + memo_id
        tracked.add(name)
        api("POST", "memos", {"content": f"# {prefix}-{label}\n\ninitial\n",
                              "visibility": "PRIVATE"}, {"memoId": memo_id})
        return name

    def remote_edit(name, body):
        api("PATCH", name, {"name": name, "content": body}, {"updateMask": "content"})

    def remote_body(name):
        return api("GET", name)["content"]

    try:
        with tempfile.TemporaryDirectory(prefix="memos-live-") as directory:
            local = Path(directory)
            data = local / "notes"
            environment = dict(os.environ, MEMOS_URL=root, TOKEN=token,
                               DATA_PATH=str(data))

            def sync(expected=0):
                result = subprocess.run([str(binary), "sync"], cwd=local, env=environment,
                                        capture_output=True, text=True, timeout=45)
                assert result.returncode == expected, (
                    f"sync exited {result.returncode}, expected {expected}: "
                    f"{result.stdout.strip()} {result.stderr.strip()}"
                )

            def note(name):
                matches = [path for path in data.rglob("note.md")
                           if not any(part.startswith(".") for part in path.relative_to(data).parts)
                           and unpack(path)[0] == name]
                assert len(matches) == 1, "Expected one local copy of the test memo"
                return matches[0]

            seed = create("seed")
            sync()
            path = note(seed)
            assert unpack(path)[1] == remote_body(seed)
            print("PASS initial download")

            edit(path, f"# {prefix}-seed\n\nlocal Vim edit\n")
            sync()
            assert remote_body(seed) == unpack(path)[1]
            remote_edit(seed, f"# {prefix}-seed\n\nphone edit\n")
            sync()
            assert unpack(path)[1] == remote_body(seed)
            print("PASS local upload and phone download")

            edit(path, f"# {prefix}-seed\n\nlocal divergent edit\n")
            remote_edit(seed, f"# {prefix}-seed\n\nphone divergent edit\n")
            sync(2)
            folder = path.parent
            diff = folder / "conflict.diff"
            assert diff.exists() and "local divergent edit" in unpack(path)[1]
            assert unpack(folder / "remote.md")[1] == remote_body(seed)
            edit(path, f"# {prefix}-seed\n\nmerged local and phone edits\n")
            diff.unlink()
            sync()
            assert remote_body(seed) == unpack(path)[1]
            assert not diff.exists()
            print("PASS conflict preservation and manual merge")

            fresh = data / "new-local" / "note.md"
            fresh.parent.mkdir()
            fresh.write_text(f"# {prefix}-local\n\ncreated in Vim\n")
            try:
                sync()
            finally:
                if fresh.exists() and fresh.read_text().startswith("---\n"):
                    tracked.add(unpack(fresh)[0])
            fresh_name = unpack(fresh)[0]
            assert remote_body(fresh_name) == unpack(fresh)[1]
            print("PASS local note creation")

            path.unlink()
            sync()
            path = note(seed)
            assert unpack(path)[1] == remote_body(seed)
            print("PASS missing local file restoration")

            api("DELETE", seed, query={"force": "true"})
            sync()
            assert not path.exists()
            assert any((data / ".memos/trash").iterdir())
            print("PASS remote deletion and local trash")

    finally:
        cleanup_errors = []
        for name in tracked:
            try:
                api("DELETE", name, query={"force": "true"}, allow_missing=True)
            except Exception as error:
                cleanup_errors.append(f"{name}: {error}")
        if cleanup_errors:
            raise RuntimeError("Test note cleanup failed: " + "; ".join(cleanup_errors))
    print(f"Live smoke test passed; cleaned up {len(tracked)} test memo IDs.")


if __name__ == "__main__":
    main()
