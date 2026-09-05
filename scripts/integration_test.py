#!/usr/bin/env python3
"""Repeatable FlashFind daemon integration suite.

Runs each case with an isolated root and XDG_DATA_HOME. The default suite
covers daemon lifecycle/recovery, endpoint isolation, SQLite init races, file
burst consistency, and repeated large-directory renames. Use --quick to skip
the 5,000-file rename stress test.
"""
from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time
from dataclasses import dataclass


@dataclass
class Sandbox:
    binary: pathlib.Path
    work: pathlib.Path
    root: pathlib.Path
    data: pathlib.Path

    @classmethod
    def create(cls, binary: pathlib.Path, label: str) -> "Sandbox":
        work = pathlib.Path(tempfile.mkdtemp(prefix=f"flashfind-{label}-"))
        root = work / "root"
        root.mkdir()
        return cls(binary, work, root, work / "data")

    @property
    def env(self) -> dict[str, str]:
        return os.environ | {"XDG_DATA_HOME": str(self.data)}

    def run(self, *args: object, capture: bool = False) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.binary, *map(str, args)],
            env=self.env,
            check=True,
            text=True,
            capture_output=capture,
        )

    def start(self) -> None:
        self.run("daemon", "--root", self.root, "start")

    def stop(self) -> None:
        subprocess.run(
            [self.binary, "daemon", "stop"],
            env=self.env,
            text=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def cleanup(self) -> None:
        self.stop()
        shutil.rmtree(self.work, ignore_errors=True)


def wait_for(predicate, label: str, timeout: float = 10.0) -> float:
    started = time.perf_counter_ns()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return (time.perf_counter_ns() - started) / 1_000_000
        time.sleep(0.0005)
    raise RuntimeError(f"timeout waiting for {label}")


def db(sandbox: Sandbox) -> sqlite3.Connection:
    path = sandbox.data / "flashfind" / "index.sqlite3"
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def indexed_count(connection: sqlite3.Connection, root: pathlib.Path, glob: str) -> int:
    return connection.execute(
        "SELECT COUNT(*) FROM files WHERE path LIKE ?", (str(root / glob),)
    ).fetchone()[0]


def test_endpoint_isolation(binary: pathlib.Path) -> str:
    a, b = Sandbox.create(binary, "endpoint-a"), Sandbox.create(binary, "endpoint-b")
    try:
        (a.root / "seed-a").write_text("a")
        (b.root / "seed-b").write_text("b")
        a.start(); b.start()
        endpoint_a = (a.data / "flashfind" / "daemon.addr").read_text().strip()
        endpoint_b = (b.data / "flashfind" / "daemon.addr").read_text().strip()
        assert endpoint_a != endpoint_b
        (a.root / "only-a.txt").write_text("a")
        (b.root / "only-b.txt").write_text("b")
        elapsed = wait_for(
            lambda: "only-a" in a.run("search", "only-a", "--limit", 5, capture=True).stdout
            and "only-b" in b.run("search", "only-b", "--limit", 5, capture=True).stdout,
            "both isolated file updates",
        )
        assert "only-b" not in a.run("search", "only-b", "--limit", 5, capture=True).stdout
        a.stop()
        assert "daemon: running" in b.run("daemon", "status", capture=True).stdout
        return f"endpoint isolation: {endpoint_a}, {endpoint_b}; updates {elapsed:.2f} ms"
    finally:
        a.cleanup(); b.cleanup()


def test_lifecycle(binary: pathlib.Path) -> str:
    s = Sandbox.create(binary, "lifecycle")
    try:
        (s.root / "seed.txt").write_text("seed")
        # Persist the root first: automatic daemon startup only attaches roots
        # registered in SQLite, not one-off --root arguments from a past run.
        s.run("index", s.root)
        s.start()
        pid_one = (s.data / "flashfind" / "daemon.pid").read_text().strip()
        assert "already running" in s.run("daemon", "start", capture=True).stdout
        s.run("daemon", "restart")
        pid_two = (s.data / "flashfind" / "daemon.pid").read_text().strip()
        assert pid_one != pid_two
        s.run("daemon", "stop")
        assert not (s.data / "flashfind" / "daemon.pid").exists()
        # Query an already indexed entry to start the daemon, then verify that
        # a subsequent filesystem event is watched and indexed.
        assert "seed" in s.run("search", "seed", "--limit", 5, capture=True).stdout
        (s.root / "auto.txt").write_text("x")
        elapsed = wait_for(
            lambda: "auto" in s.run("search", "auto", "--limit", 5, capture=True).stdout,
            "post-auto-start watch event",
        )
        return f"lifecycle: restart {pid_one}->{pid_two}; auto-start watcher {elapsed:.2f} ms"
    finally:
        s.cleanup()


def test_stale_state(binary: pathlib.Path) -> str:
    s = Sandbox.create(binary, "stale")
    try:
        state = s.data / "flashfind"
        state.mkdir(parents=True)
        (state / "daemon.pid").write_text("999999")
        (state / "daemon.addr").write_text("127.0.0.1:1")
        s.start()
        assert (state / "daemon.pid").read_text().strip() != "999999"
        assert (state / "daemon.addr").read_text().strip() != "127.0.0.1:1"
        return "stale PID/endpoint recovery"
    finally:
        s.cleanup()


def test_burst_consistency(binary: pathlib.Path) -> str:
    s = Sandbox.create(binary, "burst")
    try:
        s.run("index", s.root)
        s.start()
        connection = db(s)
        for number in range(100):
            (s.root / f"burst-{number:03}.txt").write_text("x")
        wait_for(lambda: indexed_count(connection, s.root, "burst-%") == 100, "100 creates")
        for number in range(100):
            (s.root / f"burst-{number:03}.txt").write_text("updated")
        wait_for(
            lambda: connection.execute(
                "SELECT COUNT(*) FROM files WHERE path LIKE ? AND size = 7", (str(s.root / "burst-%"),)
            ).fetchone()[0] == 100,
            "100 modifications",
        )
        for number in range(100):
            (s.root / f"burst-{number:03}.txt").rename(s.root / f"renamed-{number:03}.txt")
        wait_for(
            lambda: indexed_count(connection, s.root, "burst-%") == 0
            and indexed_count(connection, s.root, "renamed-%") == 100,
            "100 renames",
        )
        for number in range(100):
            (s.root / f"renamed-{number:03}.txt").unlink()
        wait_for(lambda: indexed_count(connection, s.root, "renamed-%") == 0, "100 deletes")
        return "100-file create/modify/rename/delete consistency"
    finally:
        s.cleanup()


def test_large_rename(binary: pathlib.Path, iterations: int) -> str:
    samples: list[float] = []
    for _ in range(iterations):
        s = Sandbox.create(binary, "large-rename")
        try:
            old = s.root / "old"
            old.mkdir()
            for number in range(5000):
                (old / f"f{number:05}.txt").write_text("x")
            s.run("index", s.root)
            s.start()
            connection = db(s)
            time.sleep(0.2)
            new = s.root / "new"
            started = time.perf_counter_ns()
            old.rename(new)
            wait_for(
                lambda: connection.execute(
                    "SELECT COUNT(*) FROM files WHERE path = ? OR path LIKE ?", (str(new), str(new) + "/%")
                ).fetchone()[0] == 5001
                and connection.execute(
                    "SELECT COUNT(*) FROM files WHERE path = ? OR path LIKE ?", (str(old), str(old) + "/%")
                ).fetchone()[0] == 0,
                "5,000-file directory rename",
            )
            samples.append((time.perf_counter_ns() - started) / 1_000_000)
        finally:
            s.cleanup()
    ordered = sorted(samples)
    return f"5,000-file rename x{iterations}: median {ordered[len(ordered)//2]:.2f} ms, max {max(samples):.2f} ms"


def test_concurrent_open(binary: pathlib.Path) -> str:
    s = Sandbox.create(binary, "concurrent-open")
    try:
        barrier = threading.Barrier(24)
        failures: list[str] = []
        def open_client() -> None:
            try:
                barrier.wait()
                s.run("roots")
            except Exception as error:
                failures.append(str(error))
        threads = [threading.Thread(target=open_client) for _ in range(24)]
        [thread.start() for thread in threads]
        [thread.join() for thread in threads]
        assert not failures, failures
        return "24 concurrent SQLite/schema opens"
    finally:
        s.cleanup()


def test_crash_recovery(binary: pathlib.Path) -> str:
    s = Sandbox.create(binary, "crash")
    try:
        s.run("index", s.root)
        s.start()
        (s.root / "before-kill.txt").write_text("x")
        wait_for(lambda: "before-kill" in s.run("search", "before-kill", "--limit", 5, capture=True).stdout, "pre-kill index")
        pid = int((s.data / "flashfind" / "daemon.pid").read_text())
        os.kill(pid, signal.SIGKILL)
        time.sleep(0.2)
        s.start()
        (s.root / "after-restart.txt").write_text("x")
        wait_for(lambda: "after-restart" in s.run("search", "after-restart", "--limit", 5, capture=True).stdout, "post-crash index")
        return "SIGKILL PID/endpoint/WAL recovery"
    finally:
        s.cleanup()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, default=pathlib.Path("target/release/flashfind"))
    parser.add_argument("--quick", action="store_true", help="skip repeated 5,000-file rename stress")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"binary not found: {binary}")
    tests = [test_endpoint_isolation, test_lifecycle, test_stale_state, test_burst_consistency]
    if not args.quick:
        tests.append(lambda current: test_large_rename(current, 10))
    tests += [test_concurrent_open, test_crash_recovery]
    passed = []
    for number, test in enumerate(tests, 1):
        try:
            result = test(binary)
            print(f"PASS {number}: {result}")
            passed.append(result)
        except Exception as error:
            print(f"FAIL {number}: {error}")
            return 1
    print(f"integration suite passed: {len(passed)}/{len(tests)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
