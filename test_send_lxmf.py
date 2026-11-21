import contextlib
from dataclasses import replace
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from urllib.parse import urlsplit

import send_lxmf as publisher


MESSAGE_HASH = "ab" * 32
DESTINATION = "e3835c698508d634b67525242309dd76"


def config_text(url):
    return f"""
[radio]
frequency_hz = 435995000

[lxmf]
enabled = true
sender_name = "R4"
destination = "{DESTINATION}"
api_base_url = "{url}"
delivery_method = "direct"
timeout_seconds = 2
retry_attempts = 3
"""


@contextlib.contextmanager
def fabric_stub():
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append((self.path, self.headers["Content-Type"], payload))
            content = payload["lxmf_message"]["content"]
            if content.endswith("reject this"):
                status, result = 400, {"message": "rejected by server"}
            elif content.endswith("uncertain result"):
                status, result = 503, {"message": "daemon accepted send without a message id"}
            else:
                status, result = 200, {"lxmf_message": {"hash": MESSAGE_HASH}}
            body = json.dumps(result).encode()
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    with HTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05})
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}", requests
        finally:
            server.shutdown()
            thread.join()


class PublisherTests(unittest.TestCase):
    def test_bss_forwarding_is_opt_in_and_excludes_raw_location(self):
        with tempfile.TemporaryDirectory() as temp:
            config = Path(temp) / "lxmf.toml"
            config.write_text(config_text("http://127.0.0.1:9337"), encoding="utf-8")
            settings = publisher.Settings.load(config)
            event = {
                "schema": "sdr.bss.v1", "source": "live", "sender": "KK7VZT",
                "text": "hello", "frequency_hz": 435_995_000,
                "timestamp": "2026-11-20T15:30:00Z",
                "location": {"raw_hex": "14C72DC7CDF1", "encoding": "unresolved"},
            }
            self.assertIsNone(settings.event_payload(event))
            forwarding = replace(settings, forward_bss=True)
            payload = forwarding.event_payload(event)
            content = payload["lxmf_message"]["content"]
            self.assertIn("BSS from KK7VZT\nhello", content)
            self.assertIn("Location field received (encoding unresolved)", content)
            self.assertNotIn(event["location"]["raw_hex"], content)
            event["location"].update(encoding="bss_fixed_point", latitude=40.25, longitude=-75.125)
            content = forwarding.event_payload(event)["lxmf_message"]["content"]
            self.assertIn("Location decoded locally", content)
            self.assertNotIn("40.25", content)
            self.assertNotIn("-75.125", content)
            self.assertIsNone(forwarding.event_payload({**event, "source": "wav"}))
            self.assertIsNone(forwarding.event_payload({**event, "schema": "sdr.bss.diagnostic.v1"}))

    def test_a_standalone_cli_and_malformed_destination(self):
        with tempfile.TemporaryDirectory() as temp, fabric_stub() as (url, requests):
            config = Path(temp) / "lxmf.toml"
            state = Path(temp) / "outbox.sqlite3"
            template = config_text(url)
            config.write_text(template, encoding="utf-8")
            command = [sys.executable, str(publisher.ROOT / "send_lxmf.py"),
                       "--config", str(config), "--state", str(state), "--text", "R4 transmission test"]

            process = subprocess.run(command, capture_output=True, text=True, timeout=10)
            self.assertEqual(process.returncode, 0, process.stderr)
            self.assertIn(f"Fabric accepted message {MESSAGE_HASH}", process.stderr)
            self.assertEqual(len(requests), 1)
            path, content_type, payload = requests[0]
            self.assertEqual(path, "/api/v1/lxmf-messages/send")
            self.assertEqual(content_type, "application/json")
            self.assertEqual(payload["delivery_method"], "direct")
            self.assertEqual(payload["lxmf_message"]["destination_hash"], DESTINATION)
            self.assertRegex(payload["lxmf_message"]["content"],
                             r"^\[R4 \| 435\.9950 MHz \| \d\d:\d\d:\d\d\]\nR4 transmission test$")

            config.write_text(template.replace(DESTINATION, "invalid hash"), encoding="utf-8")
            process = subprocess.run(command, capture_output=True, text=True, timeout=10)
            self.assertEqual(process.returncode, 2, process.stderr)
            self.assertIn("32-character hexadecimal", process.stderr)
            self.assertEqual(len(requests), 1)

    def test_b_watcher_deduplication_retry_and_uncertainty(self):
        with tempfile.TemporaryDirectory() as temp, fabric_stub() as (url, requests):
            directory = Path(temp)
            state = directory / "outbox.sqlite3"
            config = directory / "lxmf.toml"
            config.write_text(config_text(url), encoding="utf-8")
            defaults = publisher.Settings.load(config)
            settings = replace(defaults, api=urlsplit(url), sender_name="R4 café", attempts=2)
            ended_at = "2026-11-20T14:54:17.777744500Z"

            def publish_file(name, text, source="live"):
                pending = directory / f"{name}.tmp"
                pending.write_text(json.dumps({
                    "source": source, "text": text, "frequency_hz": 435_995_100, "ended_at": ended_at,
                }, ensure_ascii=False), encoding="utf-8")
                final = pending.with_suffix(".json")
                pending.rename(final)
                return publisher.file_key(final)

            def job(database, key):
                return database.execute(
                    "SELECT state, attempts, retry_at, detail FROM jobs WHERE key = ?", (key,)
                ).fetchone()

            publish_file("prior", "pre-install reception")
            with contextlib.closing(publisher.open_outbox(state)) as database:
                watcher = publisher.TranscriptWatcher(database, directory)
                publish_file("a-blank", "  \n  ")
                publish_file("b-wav", "WAV import", "wav")
                message = publish_file("c-good", "  Très bien.\nClear signal.  ")
                watcher.scan(database, settings)
                publisher.drain(database, settings)
                timestamp = datetime.fromisoformat(ended_at).astimezone().strftime("%H:%M:%S")
                self.assertEqual(requests[0][2]["lxmf_message"]["content"],
                                 f"[R4 café | 435.9951 MHz | {timestamp}]\n  Très bien.\nClear signal.  ")
                self.assertEqual(job(database, message)[:2], ("accepted", 1))

            offline = publish_file("d-outage", "captured while publisher was stopped")
            with contextlib.closing(publisher.open_outbox(state)) as database, socket.socket() as unavailable:
                unavailable.bind(("127.0.0.1", 0))
                watcher = publisher.TranscriptWatcher(database, directory)
                watcher.scan(database, settings)
                offline_url = urlsplit(f"http://127.0.0.1:{unavailable.getsockname()[1]}")
                offline_settings = replace(settings, api=offline_url, timeout=0.25)
                with self.assertLogs(level="WARNING") as outage:
                    self.assertTrue(publisher.send_due(database, offline_settings, offline))
                    self.assertEqual(job(database, offline)[:2], ("pending", 1))
                    self.assertGreater(job(database, offline)[2], time.time())
                    with database:
                        database.execute("UPDATE jobs SET retry_at = 0 WHERE key = ?", (offline,))
                    publisher.drain(database, offline_settings, offline)
                self.assertIn("retained for --retry-pending", "\n".join(outage.output))
                self.assertEqual(job(database, offline)[:2], ("deferred", 2))
                self.assertEqual(len(requests), 1)

            with contextlib.closing(publisher.open_outbox(state)) as database:
                result = publisher.run(SimpleNamespace(watch=False, retry_pending=True), settings, database)
                self.assertEqual(result, 0)
                self.assertEqual(job(database, offline)[:2], ("accepted", 1))
                watcher = publisher.TranscriptWatcher(database, directory)
                rejected = publish_file("e-rejected", "reject this")
                uncertain = publish_file("f-uncertain", "uncertain result")
                later = publish_file("g-later", "a subsequent transmission")
                interrupted = publish_file("h-interrupted", "interrupted after POST")
                watcher.scan(database, settings)
                with database:
                    database.execute("UPDATE jobs SET state = 'sending', attempts = 1 WHERE key = ?",
                                     (interrupted,))
                with self.assertLogs(level="ERROR"):
                    publisher.drain(database, settings)
                self.assertEqual(job(database, rejected)[:2], ("rejected", 1))
                self.assertEqual(job(database, uncertain)[:2], ("ambiguous", 1))
                self.assertEqual(job(database, later)[:2], ("accepted", 1))
                self.assertEqual(len(requests), 5)

            with contextlib.closing(publisher.open_outbox(state)) as database:
                watcher = publisher.TranscriptWatcher(database, directory)
                watcher.scan(database, settings)
                publisher.drain(database, settings)
                self.assertEqual(len(requests), 5)
                self.assertEqual(job(database, interrupted)[:2], ("sending", 1))


if __name__ == "__main__":
    unittest.main()
