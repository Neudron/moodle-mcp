"""Run: uv run scripts/test_icloud_import.py (pyicloud needed for the jar class)."""
import importlib.util, io, json, os, sqlite3, subprocess, sys, tempfile, unittest, contextlib
from pathlib import Path

HERE = Path(__file__).parent
spec = importlib.util.spec_from_file_location("imp", HERE / "icloud_import_cookies.py")
imp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(imp)


def make_profile(root: Path, rows, wal_only=False):
    prof = root / "p.default"
    prof.mkdir(parents=True)
    (root / "installs.ini").write_text("[X]\nDefault=p.default\nLocked=1\n")
    db = prof / "cookies.sqlite"
    con = sqlite3.connect(db)
    con.execute("PRAGMA journal_mode=WAL")
    con.execute("CREATE TABLE moz_cookies(name,value,host,path,expiry,isSecure)")
    if wal_only:
        con.commit()
        con.execute("PRAGMA wal_autocheckpoint=0")
    con.executemany("INSERT INTO moz_cookies VALUES(?,?,?,?,?,?)", rows)
    con.commit()
    return con  # keep open so the rows stay WAL-only


ROWS = [
    ("X-APPLE-WEBAUTH-TOKEN", "FAKE", ".icloud.com", "/", 1_900_000_000_000, 1),
    ("X-APPLE-DS-WEB-SESSION-TOKEN", "SEKRET9", ".icloud.com", "/", 1_900_000_000, 1),
    ("other", "x", ".example.com", "/", 1_900_000_000, 1),
]


class T(unittest.TestCase):
    def setUp(self):
        self.td = tempfile.TemporaryDirectory()
        self.root = Path(self.td.name)
        self.addCleanup(self.td.cleanup)

    def env(self, **kw):
        state = self.root / "state"
        jar = state / "pyicloud"
        jar.mkdir(parents=True)
        (state / "reminders-auth-required").touch()
        e = dict(FIREFOX_DIR=str(self.root / "ff"), ICLOUD_GLASS_STATE=str(state),
                 ICLOUD_IMPORT_ACCOUNT="a@icloud.com", **kw)
        self.enter = lambda: [os.environ.__setitem__(k, v) for k, v in e.items()]
        self.enter()
        return state, jar

    def run_main(self, *a):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = imp.main(list(a))
        return rc, out.getvalue()

    def test_wal_only_host_filter_ms(self):
        con = make_profile(self.root / "ff", ROWS, wal_only=True)
        self.env()
        self.assertTrue(list((self.root / "ff/p.default").glob("*.sqlite-wal")))
        cookies = imp.load_cookies()
        con.close()
        self.assertEqual({c["name"] for c in cookies}, {"X-APPLE-WEBAUTH-TOKEN", "X-APPLE-DS-WEB-SESSION-TOKEN"})
        self.assertEqual(imp.token_expiry(cookies), 1_900_000_000)  # ms -> s

    def test_ok_clears_marker_and_never_prints_values(self):
        con = make_profile(self.root / "ff", ROWS)
        state, jar = self.env(ICLOUD_IMPORT_STUB_VALIDATE="ok")
        rc, out = self.run_main()
        con.close()
        self.assertEqual(rc, 0)
        self.assertFalse((state / "reminders-auth-required").exists())
        self.assertNotIn("FAKE", out)
        self.assertNotIn("SEKRET9", out)
        s = json.loads((jar / "aicloudcom.session").read_text())
        self.assertEqual(s["session_token"], "SEKRET9")

    def test_failed_validation_rolls_back(self):
        con = make_profile(self.root / "ff", ROWS)
        state, jar = self.env(ICLOUD_IMPORT_STUB_VALIDATE="fail")
        (jar / "aicloudcom.session").write_text('{"session_token":"OLD"}')
        (jar / "aicloudcom.cookiejar").write_text("#LWP-Cookies-2.0\n")
        rc, out = self.run_main()
        con.close()
        self.assertEqual(rc, 2)
        self.assertEqual(json.loads(out)["kind"], "auth")
        self.assertTrue((state / "reminders-auth-required").exists())
        self.assertEqual(json.loads((jar / "aicloudcom.session").read_text())["session_token"], "OLD")
        self.assertEqual((jar / "aicloudcom.cookiejar").read_text(), "#LWP-Cookies-2.0\n")

    def test_next_profile_when_first_has_no_icloud(self):
        root = self.root / "ff"
        make_profile(root, [("o", "x", ".example.com", "/", 1, 1)]).close()
        p2 = root / "q.other"
        p2.mkdir()
        c = sqlite3.connect(p2 / "cookies.sqlite")
        c.execute("CREATE TABLE moz_cookies(name,value,host,path,expiry,isSecure)")
        c.executemany("INSERT INTO moz_cookies VALUES(?,?,?,?,?,?)", ROWS)
        c.commit(); c.close()
        (root / "profiles.ini").write_text("[P]\nPath=q.other\nDefault=1\n")
        self.env()
        self.assertEqual(imp.token_expiry(imp.load_cookies()), 1_900_000_000)

    def test_locked_live_db_does_not_hang(self):
        prof = self.root / "ff" / "p.default"
        prof.mkdir(parents=True)
        (self.root / "ff" / "installs.ini").write_text("[X]\nDefault=p.default\n")
        live = sqlite3.connect(prof / "cookies.sqlite")
        live.execute("PRAGMA locking_mode=EXCLUSIVE")
        live.execute("PRAGMA journal_mode=WAL")
        live.execute("CREATE TABLE moz_cookies(name,value,host,path,expiry,isSecure)")
        live.execute("INSERT INTO moz_cookies VALUES(?,?,?,?,?,?)", ROWS[0])
        live.commit()
        self.addCleanup(live.close)
        env = dict(os.environ, FIREFOX_DIR=str(self.root / "ff"))
        r = subprocess.run([sys.executable, str(HERE / "icloud_import_cookies.py"), "--check"],
                           env=env, capture_output=True, text=True, timeout=10)  # hang -> TimeoutExpired
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "1900000000"))

    def test_no_profile_exit_3(self):
        self.env()
        rc, out = self.run_main()
        self.assertEqual(rc, 3)

    def test_check_prints_epoch_only(self):
        con = make_profile(self.root / "ff", ROWS)
        self.env()
        rc, out = self.run_main("--check")
        con.close()
        self.assertEqual((rc, out.strip()), (0, "1900000000"))


if __name__ == "__main__":
    unittest.main()
