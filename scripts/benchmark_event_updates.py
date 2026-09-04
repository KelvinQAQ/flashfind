#!/usr/bin/env python3
"""End-to-end event-indexing benchmark for FlashFind.

The generated root has a 3-way, 6-level tree and 25 files in every directory
(about 28k indexed entries).  The FlashFind data directory is deliberately
inside that root, so this also exercises the WAL-event self-watch regression.

The script polls SQLite directly at 0.5 ms intervals.  It measures the time
from a filesystem operation until the expected committed index state is
visible, avoiding CLI startup/IPC time in the primary measurement.
"""

from __future__ import annotations

import argparse
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from collections import defaultdict
from statistics import median

POLL_SECONDS = 0.0005
TIMEOUT_SECONDS = 20.0
FANOUT = 3
MAX_DEPTH = 6
FILES_PER_DIRECTORY = 25
FILE_DEPTHS = (1, 3, 6)
DIRECTORY_DEPTHS = (2, 4, 6)


def make_tree(root: Path) -> int:
    """Build a deterministic tree and return its expected entry count."""
    count = 0
    pending = [(root, 0)]
    while pending:
        directory, depth = pending.pop()
        directory.mkdir(parents=True, exist_ok=True)
        count += 1
        for number in range(FILES_PER_DIRECTORY):
            (directory / f"seed-{depth}-{number:02}.dat").write_bytes(b"x")
            count += 1
        if depth < MAX_DEPTH:
            for branch in range(FANOUT):
                pending.append((directory / f"d{branch}", depth + 1))
    return count


def deep_directory(root: Path, depth: int) -> Path:
    path = root
    for _ in range(depth):
        path /= "d0"
    return path


def subtree_count(connection: sqlite3.Connection, path: Path) -> int:
    text = str(path)
    return connection.execute(
        """SELECT COUNT(*) FROM files WHERE path = ?1 OR
           (substr(path, 1, length(?1)) = ?1 AND
            substr(path, length(?1) + 1, 1) = ?2)""",
        (text, os.sep),
    ).fetchone()[0]


def file_size(connection: sqlite3.Connection, path: Path) -> int | None:
    row = connection.execute("SELECT size FROM files WHERE path = ?", (str(path),)).fetchone()
    return None if row is None else row[0]


def wait_for(connection: sqlite3.Connection, predicate, label: str) -> float:
    start = time.perf_counter_ns()
    deadline = time.monotonic() + TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if predicate():
            return (time.perf_counter_ns() - start) / 1_000_000
        time.sleep(POLL_SECONDS)
    raise TimeoutError(f"timed out waiting for {label}")


def make_subtree(staging_parent: Path, name: str) -> tuple[Path, int]:
    root = staging_parent / name
    root.mkdir()
    count = 1
    for directory in (root / "a", root / "b", root / "a" / "nested"):
        directory.mkdir(parents=True, exist_ok=True)
        count += 1
        for file_number in range(10):
            (directory / f"payload-{file_number:02}.txt").write_bytes(b"payload")
            count += 1
    for file_number in range(10):
        (root / f"root-{file_number:02}.txt").write_bytes(b"payload")
        count += 1
    return root, count


def percentile(values: list[float], percent: float) -> float:
    values = sorted(values)
    index = min(len(values) - 1, round((len(values) - 1) * percent))
    return values[index]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/release/flashfind"))
    parser.add_argument(
        "--workdir", type=Path, default=None, help="keep benchmark files here (default: temporary directory)"
    )
    parser.add_argument("--keep", action="store_true", help="do not delete benchmark files")
    parser.add_argument(
        "--iterations", type=int, default=5, help="samples per operation/depth (default: 5)"
    )
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"binary does not exist: {binary}")
    if args.iterations < 1:
        parser.error("--iterations must be at least 1")

    temporary = None
    if args.workdir is None:
        temporary = tempfile.TemporaryDirectory(prefix="flashfind-event-bench-")
        workdir = Path(temporary.name)
    else:
        workdir = args.workdir.resolve()
        shutil.rmtree(workdir, ignore_errors=True)
        workdir.mkdir(parents=True)
    root = workdir / "root"
    data_home = root / "data"
    log = workdir / "daemon.log"
    environment = os.environ | {"XDG_DATA_HOME": str(data_home)}
    daemon: subprocess.Popen[str] | None = None

    try:
        expected_seed_entries = make_tree(root)
        print(
            f"root={root}\n"
            f"tree: fanout={FANOUT}, max_depth={MAX_DEPTH}, files_per_directory={FILES_PER_DIRECTORY}, "
            f"seed_entries={expected_seed_entries}\n"
            f"data directory (inside root): {data_home / 'flashfind'}"
        )
        subprocess.run([binary, "index", root], env=environment, check=True, text=True)
        database = data_home / "flashfind" / "index.sqlite3"
        connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5)
        initial_entries = connection.execute("SELECT COUNT(*) FROM files").fetchone()[0]
        print(f"initial indexed entries: {initial_entries}")

        with log.open("w") as output:
            daemon = subprocess.Popen([binary, "daemon"], env=environment, stdout=output, stderr=subprocess.STDOUT, text=True)
        # Give recursive inotify watch registration time to complete. The root
        # has only 1,093 directories, but this also avoids measuring startup.
        time.sleep(1.0)
        if daemon.poll() is not None:
            raise RuntimeError(f"daemon exited early; log:\n{log.read_text()}")

        results: list[tuple[str, int, float]] = []
        staging_parent = workdir / "staging"
        staging_parent.mkdir()
        for iteration in range(args.iterations):
            for depth in FILE_DEPTHS:
                parent = deep_directory(root, depth)
                created = parent / f"bench-file-create-d{depth}-i{iteration}.txt"
                created.write_bytes(b"x" * 17)
                results.append(("file-create", depth, wait_for(connection, lambda p=created: file_size(connection, p) == 17, str(created))))

                created.write_bytes(b"x" * 8193)
                results.append(("file-modify", depth, wait_for(connection, lambda p=created: file_size(connection, p) == 8193, str(created))))

                renamed = parent / f"bench-file-renamed-d{depth}-i{iteration}.txt"
                created.rename(renamed)
                results.append(("file-rename", depth, wait_for(connection, lambda a=created, b=renamed: file_size(connection, a) is None and file_size(connection, b) == 8193, str(renamed))))

                renamed.unlink()
                results.append(("file-delete", depth, wait_for(connection, lambda p=renamed: file_size(connection, p) is None, str(renamed))))

            for depth in DIRECTORY_DEPTHS:
                parent = deep_directory(root, depth)
                staged, expected_entries = make_subtree(staging_parent, f"staged-d{depth}-i{iteration}")
                inserted = parent / f"bench-directory-added-d{depth}-i{iteration}"
                staged.rename(inserted)
                results.append(("directory-add-subtree", depth, wait_for(connection, lambda p=inserted, n=expected_entries: subtree_count(connection, p) == n, str(inserted))))

                renamed = parent / f"bench-directory-renamed-d{depth}-i{iteration}"
                inserted.rename(renamed)
                results.append(("directory-rename-subtree", depth, wait_for(connection, lambda a=inserted, b=renamed, n=expected_entries: subtree_count(connection, a) == 0 and subtree_count(connection, b) == n, str(renamed))))

                shutil.rmtree(renamed)
                results.append(("directory-delete-subtree", depth, wait_for(connection, lambda p=renamed: subtree_count(connection, p) == 0, str(renamed))))

        grouped: dict[tuple[str, int], list[float]] = defaultdict(list)
        for operation, depth, elapsed in results:
            grouped[(operation, depth)].append(elapsed)
        print("\noperation                     depth    n    min  median    p95    max (ms)")
        print("--------------------------------------------------------------------------")
        for (operation, depth), samples in sorted(grouped.items()):
            print(
                f"{operation:29} {depth:>5}  {len(samples):>3}  {min(samples):>5.2f}  "
                f"{median(samples):>6.2f}  {percentile(samples, 0.95):>5.2f}  {max(samples):>5.2f}"
            )
        all_latencies = [elapsed for _, _, elapsed in results]
        final_entries = connection.execute("SELECT COUNT(*) FROM files").fetchone()[0]
        if final_entries != initial_entries:
            raise RuntimeError(
                f"index count changed after balanced operations: initial={initial_entries}, final={final_entries}"
            )
        print(
            f"\nsummary: n={len(all_latencies)}, min={min(all_latencies):.2f} ms, "
            f"median={median(all_latencies):.2f} ms, p95={percentile(all_latencies, 0.95):.2f} ms, "
            f"max={max(all_latencies):.2f} ms; final entries={final_entries} (unchanged)"
        )
        if log.read_text():
            print(f"\ndaemon log:\n{log.read_text()}")
        return 0
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=3)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
        if temporary is not None and not args.keep:
            temporary.cleanup()
        elif not args.keep:
            shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        raise
