"""Independent Python/Rust ownership interlock, without external services."""
import sys
import sqlite3
import tempfile
import unittest
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import PlaytimeError
from playtime.jobs import RunLock, ensure_idle


class ServiceAdmissionTests(unittest.TestCase):
    def test_durable_reservation_blocks_legacy_before_start_and_until_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / '.playtime').mkdir()
            path = root / '.playtime/service.sqlite3'
            with sqlite3.connect(path) as db:
                db.execute('CREATE TABLE runs(active INTEGER)')
                db.execute('INSERT INTO runs VALUES(1)')
            with RunLock(root):
                with self.assertRaisesRegex(PlaytimeError, 'reserved run'):
                    ensure_idle(root)
            with sqlite3.connect(path) as db:
                db.execute('UPDATE runs SET active=NULL')
            with RunLock(root):
                ensure_idle(root)

    def test_damaged_ledger_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / '.playtime').mkdir()
            (root / '.playtime/service.sqlite3').write_bytes(b'damaged')
            with RunLock(root):
                with self.assertRaisesRegex(PlaytimeError, 'unresolved'):
                    ensure_idle(root)
