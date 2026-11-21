"""Publish completed R3AKT Sigil transcripts through Fabric's HTTP API."""

import argparse
from contextlib import closing
from dataclasses import dataclass
from datetime import datetime
import http.client
import json
import logging
import math
import os
from pathlib import Path
import re
import sqlite3
import time
import tomllib
from urllib.parse import SplitResult, urlsplit
from uuid import uuid4


ROOT = Path(__file__).resolve().parent
SEND_PATH = "/api/v1/lxmf-messages/send"
POLL_SECONDS = 0.5
MAX_RESPONSE_BYTES = 1024 * 1024


def frequency(value):
    if type(value) is not int or not 1 <= value <= 2**32 - 1:
        raise ValueError("frequency_hz must be a positive 32-bit integer")
    return value


@dataclass(frozen=True)
class Settings:
    enabled: bool
    frequency_hz: int
    sender_name: str
    destination: str
    api: SplitResult
    method: str
    timeout: float
    attempts: int
    forward_bss: bool = False

    @classmethod
    def load(cls, path):
        with path.open("rb") as config_file:
            config = tomllib.load(config_file)
        radio, lxmf = config["radio"], config["lxmf"]
        enabled = lxmf["enabled"]
        if type(enabled) is not bool:
            raise ValueError("lxmf.enabled must be a boolean")
        sender = lxmf["sender_name"]
        if not isinstance(sender, str) or not sender.strip() or sender.splitlines() != [sender]:
            raise ValueError("lxmf.sender_name must be a nonblank single line")
        destination = lxmf["destination"]
        if not isinstance(destination, str) or not re.fullmatch(r"[0-9a-fA-F]{32}", destination):
            raise ValueError("lxmf.destination must be a 32-character hexadecimal LXMF address")
        raw_url = lxmf["api_base_url"]
        if not isinstance(raw_url, str):
            raise ValueError("lxmf.api_base_url must be an HTTP(S) base URL")
        api = urlsplit(raw_url)
        if (api.scheme not in ("http", "https") or not api.hostname or api.username is not None
                or api.query or api.fragment or api.port == 0):
            raise ValueError("lxmf.api_base_url must be an HTTP(S) base URL without credentials, query or fragment")
        method = lxmf["delivery_method"]
        if method not in ("direct", "opportunistic", "propagated"):
            raise ValueError("lxmf.delivery_method must be direct, opportunistic or propagated")
        timeout = lxmf["timeout_seconds"]
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("lxmf.timeout_seconds must be positive and finite")
        attempts = lxmf["retry_attempts"]
        if type(attempts) is not int or attempts < 1:
            raise ValueError("lxmf.retry_attempts must be a positive integer")
        forward_bss = lxmf.get("forward_bss", False)
        if type(forward_bss) is not bool:
            raise ValueError("lxmf.forward_bss must be a boolean")
        return cls(enabled, frequency(radio["frequency_hz"]), sender, destination.lower(),
                   api, method, timeout, attempts, forward_bss)

    def payload(self, text, hz, ended_at):
        timestamp = ended_at.astimezone().strftime("%H:%M:%S")
        return {
            "delivery_method": self.method,
            "lxmf_message": {
                "destination_hash": self.destination,
                "content": f"[{self.sender_name} | {hz / 1_000_000:.4f} MHz | {timestamp}]\n{text}",
            },
        }

    def event_payload(self, record):
        if record["source"] != "live":
            return None
        schema = record.get("schema")
        if schema == "sdr.bss.v1" and self.forward_bss:
            body = f'BSS from {record.get("sender") or "(unknown sender)"}'
            if record.get("text"):
                body += f'\n{record["text"]}'
            if location := record.get("location"):
                if location.get("encoding") == "bss_fixed_point":
                    body += "\nLocation decoded locally"
                else:
                    body += "\nLocation field received (encoding unresolved)"
            timestamp = record["timestamp"]
        elif schema is None and record["text"].strip():
            body = record["text"]
            timestamp = record["ended_at"]
        else:
            return None
        received_at = datetime.fromisoformat(timestamp)
        if received_at.tzinfo is None:
            raise ValueError("timestamp must include a timezone")
        return self.payload(body, frequency(record["frequency_hz"]), received_at)


def submit(settings, payload):
    """Return an outbox state and its details. Only pre-POST failure is retryable."""
    connection = http.client.HTTPSConnection if settings.api.scheme == "https" else http.client.HTTPConnection
    with closing(connection(settings.api.hostname, settings.api.port, timeout=settings.timeout)) as client:
        try:
            client.connect()
        except OSError as error:
            return "retry", str(error)

        try:
            client.request("POST", settings.api.path.rstrip("/") + SEND_PATH,
                           body=payload.encode("utf-8"),
                           headers={"Content-Type": "application/json", "Accept": "application/json"})
            with client.getresponse() as response:
                status = response.status
                data = response.read(MAX_RESPONSE_BYTES + 1)
        except (OSError, http.client.HTTPException) as error:
            return "ambiguous", f"connection failed during POST: {error}"

    try:
        body = json.loads(data) if len(data) <= MAX_RESPONSE_BYTES else None
    except (UnicodeError, ValueError):
        body = None
    if 200 <= status < 300:
        message = body.get("lxmf_message") if isinstance(body, dict) else None
        message_id = message.get("hash") if isinstance(message, dict) else None
        if isinstance(message_id, str) and message_id.strip():
            return "accepted", message_id
        return "ambiguous", f"HTTP {status} without lxmf_message.hash"

    reason = body.get("message") if isinstance(body, dict) else None
    detail = f"HTTP {status}"
    if isinstance(reason, str):
        detail += f" {' '.join(reason.split())[:240]}"
    # Fabric can return 503 after handing the message to the daemon.
    return ("rejected" if 400 <= status < 500 else "ambiguous"), detail


def open_outbox(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    database = sqlite3.connect(path)
    database.executescript("""
        CREATE TABLE IF NOT EXISTS watched_directories (path TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS jobs (
            key TEXT PRIMARY KEY,
            payload TEXT,
            state TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            retry_at REAL NOT NULL DEFAULT 0,
            detail TEXT
        );
    """)
    return database


def file_key(path):
    return "file:" + os.path.normcase(str(path))


def insert_job(database, key, payload):
    database.execute("INSERT OR IGNORE INTO jobs (key, payload, state) VALUES (?, ?, ?)",
                     (key, json.dumps(payload) if payload is not None else None,
                      "pending" if payload is not None else "ignored"))


class TranscriptWatcher:
    def __init__(self, database, directory):
        self.directory = directory.resolve()
        self.directory.mkdir(parents=True, exist_ok=True)
        with database:
            first_run = database.execute(
                "INSERT OR IGNORE INTO watched_directories VALUES (?)",
                (os.path.normcase(str(self.directory)),),
            ).rowcount
            if first_run:
                historical = [(file_key(path),) for path in self.directory.glob("*.json")]
                database.executemany("INSERT OR IGNORE INTO jobs (key, state) VALUES (?, 'ignored')",
                                     historical)
                logging.info("First watch of %s, skipped %d existing file(s)", self.directory, len(historical))
        self.seen = {key for (key,) in database.execute("SELECT key FROM jobs WHERE key LIKE 'file:%'")}

    def scan(self, database, settings):
        for path in sorted(self.directory.glob("*.json")):
            key = file_key(path)
            if key in self.seen:
                continue
            try:
                record = json.loads(path.read_text(encoding="utf-8"))
                payload = settings.event_payload(record)
            except OSError as error:
                logging.warning("Could not read %s, will check again: %s", path.name, error)
                continue
            except (AttributeError, KeyError, TypeError, ValueError) as error:
                logging.warning("Skipped malformed transcript %s: %s", path.name, error)
                payload = None
            with database:
                insert_job(database, key, payload)
            self.seen.add(key)


def send_due(database, settings, key=None):
    with database:
        job = database.execute("""
            UPDATE jobs SET state = 'sending', attempts = attempts + 1
            WHERE key = (
                SELECT key FROM jobs WHERE state = 'pending' AND retry_at <= ?
                AND (? IS NULL OR key = ?) ORDER BY retry_at, rowid LIMIT 1
            )
            RETURNING key, payload, attempts
        """, (time.time(), key, key)).fetchone()
    if job is None:
        return False
    # "sending" survives interruptions, so a request that may have gone out
    # can never be replayed merely by restarting or running another publisher.
    job_key, payload, attempts = job
    destination = json.loads(payload)["lxmf_message"]["destination_hash"]
    label = Path(job_key.removeprefix("file:")).name
    logging.info("Submitting %s to %s (attempt %d)", label, destination, attempts)
    state, detail = submit(settings, payload)
    retry_at = 0
    if state == "retry":
        if attempts < settings.attempts:
            delay = min(2 ** min(attempts - 1, 5), 30)
            retry_at = time.time() + delay
            state = "pending"
            logging.warning("%s: connection failed before POST, retry in %ds: %s", label, delay, detail)
        else:
            state = "deferred"
            logging.warning("%s: connection failed before POST, retained for --retry-pending: %s", label, detail)
    elif state == "accepted":
        logging.info("%s: Fabric accepted message %s", label, detail)
    elif state == "rejected":
        logging.error("%s: Fabric rejected message (%s)", label, detail)
    else:
        logging.error("%s: submission uncertain, held for review (%s)", label, detail)
    with database:
        database.execute("UPDATE jobs SET state = ?, retry_at = ?, detail = ? WHERE key = ?",
                         (state, retry_at, detail, job_key))
    return True


def drain(database, settings, key=None):
    while True:
        if send_due(database, settings, key):
            continue
        (next_retry,) = database.execute(
            "SELECT min(retry_at) FROM jobs WHERE state = 'pending' AND (? IS NULL OR key = ?)",
            (key, key),
        ).fetchone()
        if next_retry is None:
            return
        time.sleep(max(0, min(POLL_SECONDS, next_retry - time.time())))


def run(args, settings, database):
    if args.watch:
        watcher = TranscriptWatcher(database, args.watch_dir)
        logging.info("Watching %s for completed live transcripts", watcher.directory)
        while True:
            watcher.scan(database, settings)
            if not send_due(database, settings):
                time.sleep(POLL_SECONDS)
    if args.retry_pending:
        with database:
            count = database.execute(
                "UPDATE jobs SET state = 'pending', attempts = 0, retry_at = 0 WHERE state = 'deferred'"
            ).rowcount
        logging.info("Requeued %d safely deferred message(s)", count)
        drain(database, settings)
        remaining = database.execute("SELECT count(*) FROM jobs WHERE state = 'deferred'").fetchone()[0]
        return int(remaining > 0)
    if not args.text.strip():
        logging.info("Skipped blank text")
        return 0
    key = uuid4().hex
    with database:
        insert_job(database, key, settings.payload(args.text, settings.frequency_hz, datetime.now().astimezone()))
    drain(database, settings, key)
    (state,) = database.execute("SELECT state FROM jobs WHERE key = ?", (key,)).fetchone()
    return int(state != "accepted")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--watch", action="store_true", help="publish new live transcript JSON files")
    mode.add_argument("--text", help="send one manually supplied test message")
    mode.add_argument("--retry-pending", action="store_true", help="retry safely deferred messages")
    parser.add_argument("--config", type=Path, default=ROOT / "lxmf.toml")
    parser.add_argument("--watch-dir", type=Path, default=ROOT / "transcripts")
    parser.add_argument("--state", type=Path, default=ROOT / ".lxmf-outbox.sqlite3")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="[%(asctime)s] %(message)s", datefmt="%H:%M:%S")
    try:
        settings = Settings.load(args.config)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.error(f"{args.config}: {error}")
    if not settings.enabled:
        logging.info("LXMF publishing is disabled in %s", args.config)
        return 0
    try:
        with closing(open_outbox(args.state)) as database:
            held = database.execute(
                "SELECT count(*) FROM jobs WHERE state IN ('ambiguous', 'sending')"
            ).fetchone()[0]
            if held:
                logging.warning("Outbox has %d uncertain or in-flight submission(s), automatic replay is disabled", held)
            return run(args, settings, database)
    except (OSError, sqlite3.Error) as error:
        logging.error("Publisher stopped: %s", error)
        return 1
    except KeyboardInterrupt:
        logging.info("Publisher stopped")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
