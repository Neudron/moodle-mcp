#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["pyicloud==2.7.0", "rich>=15"]
# ///
"""Import live Firefox icloud.com cookies into icloud-glass' pyicloud session.

No password, no 2FA. Never prints cookie values (names, expiry, counts only).
Exit: 0 ok | 2 stale cookies (JSON on stdout) | 3 no profile/cookies.
Modes: (none) import | --dry-run trial in a temp dir, touches nothing | --check print token expiry epoch.
Env (tests): FIREFOX_DIR, ICLOUD_GLASS_STATE, ICLOUD_IMPORT_JAR_DIR, ICLOUD_IMPORT_ACCOUNT, ICLOUD_IMPORT_STUB_VALIDATE=ok|fail.
"""
from __future__ import annotations

import configparser, contextlib, fcntl, json, os, shutil, sqlite3, sys, tempfile, time
from http.cookiejar import Cookie
from pathlib import Path

TOKEN = "X-APPLE-WEBAUTH-TOKEN"
SESSION_COOKIE = "X-APPLE-DS-WEB-SESSION-TOKEN"


class NoData(Exception):
    pass


def firefox_roots() -> list[Path]:
    if os.environ.get("FIREFOX_DIR"):
        return [Path(os.environ["FIREFOX_DIR"])]
    h = Path.home()
    return [h / ".config/mozilla/firefox", h / ".mozilla/firefox"]


def find_profiles() -> list[Path]:
    out: list[Path] = []
    for root in firefox_roots():
        for name in ("installs.ini", "profiles.ini"):
            ini = root / name
            if not ini.exists():
                continue
            cp = configparser.ConfigParser(interpolation=None)
            cp.read(ini)
            for s in cp.sections():
                rel = None
                if name == "installs.ini":
                    rel = cp[s].get("Default")
                    is_rel = True
                elif cp[s].get("Default") == "1" or s.startswith("Install"):
                    rel = cp[s].get("Path") or cp[s].get("Default")
                    is_rel = cp[s].get("IsRelative", "1") == "1"
                if rel:
                    p = (root / rel) if is_rel else Path(rel)
                    if (p / "cookies.sqlite").exists() and p not in out:
                        out.append(p)
    return out


def load_cookies() -> list[dict]:
    """First profile that has icloud.com cookies."""
    for p in find_profiles():
        try:
            return read_cookies(p)
        except (NoData, sqlite3.Error):
            continue
    raise NoData("no firefox profile with icloud.com cookies")


def read_cookies(profile: Path) -> list[dict]:
    """Copy db+wal+shm to a 0700 temp dir (Firefox holds the live DB exclusive) and read icloud.com rows."""
    with tempfile.TemporaryDirectory() as td:
        os.chmod(td, 0o700)
        for suf in ("", "-wal", "-shm"):
            f = profile / f"cookies.sqlite{suf}"
            if f.exists():
                shutil.copy2(f, Path(td) / f.name)
        con = sqlite3.connect(f"file:{td}/cookies.sqlite?mode=ro", uri=True)
        try:
            rows = con.execute(
                "SELECT name,value,host,path,expiry,isSecure FROM moz_cookies "
                "WHERE host LIKE '%icloud.com' OR host LIKE '%icloud.com.cn'"
            ).fetchall()
        finally:
            con.close()
    out = []
    for name, value, host, path, expiry, secure in rows:
        exp = int(expiry or 0)
        if exp > 10**11:  # older Firefox stored ms
            exp //= 1000
        out.append(dict(name=name, value=value, host=host, path=path, expiry=exp, secure=bool(secure)))
    if not out:
        raise NoData("no icloud.com cookies in firefox")
    return out


def token_expiry(cookies: list[dict]) -> int:
    return max((c["expiry"] for c in cookies if c["name"] == TOKEN), default=0)


def build_jar(cookies: list[dict], jar_dir: Path, account_name: str):
    from pyicloud.cookie_jar import PyiCloudCookieJar

    fn = "".join(c for c in account_name if c.isalnum() or c == "_") + ".cookiejar"
    jar = PyiCloudCookieJar(filename=str(jar_dir / fn))
    for c in cookies:
        jar.set_cookie(Cookie(
            0, c["name"], c["value"], None, False, c["host"], True, c["host"].startswith("."),
            c["path"] or "/", True, c["secure"], c["expiry"] or None, not c["expiry"], None, None, {},
        ))
    jar.save(ignore_discard=True, ignore_expires=True)
    os.chmod(jar.filename, 0o600)
    return jar


def account_name() -> str:
    p = os.environ.get("ICLOUD_IMPORT_ACCOUNT")
    if p:
        return p
    cfg = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / "icloud-glass" / "account"
    if not cfg.exists():
        raise NoData("no icloud-glass account file")
    return cfg.read_text().strip()


def validate(jar_dir: Path, acct: str) -> tuple[bool, str, int]:
    """Returns (ok, reason, list_count). Never logs in: get_auth_status only."""
    stub = os.environ.get("ICLOUD_IMPORT_STUB_VALIDATE")
    if stub:
        return stub == "ok", "stub", 0
    from pyicloud import PyiCloudService

    api = PyiCloudService(acct, None, cookie_directory=str(jar_dir), authenticate=False)
    st = api.get_auth_status()
    if not st["authenticated"]:
        return False, "session rejected by icloud", 0
    if st["requires_2fa"] or not st["trusted_session"]:
        return False, "2fa or untrusted session", 0
    try:
        return True, "ok", len(list(api.reminders.lists()))
    except Exception as e:  # noqa: BLE001
        return False, f"reminders.lists failed: {type(e).__name__}", 0


def seed_session(jar_dir: Path, acct: str, cookies: list[dict], src_dir: Path | None):
    fn = "".join(c for c in acct if c.isalnum() or c == "_") + ".session"
    data = {}
    if src_dir and (src_dir / fn).exists():
        try:
            data = json.loads((src_dir / fn).read_text())
        except ValueError:
            data = {}
    tok = next((c["value"] for c in cookies if c["name"] == SESSION_COOKIE), None)
    if tok:
        data["session_token"] = tok
    (jar_dir / fn).write_text(json.dumps(data))
    os.chmod(jar_dir / fn, 0o600)
    return bool(tok)


def main(argv: list[str]) -> int:
    try:
        cookies = load_cookies()
        if "--check" in argv:
            print(token_expiry(cookies))
            return 0
        acct = account_name()
    except NoData as e:
        print(json.dumps({"ok": False, "kind": "missing", "msg": str(e)}))
        return 3
    state = Path(os.environ.get("ICLOUD_GLASS_STATE") or Path.home() / ".local/share/icloud-glass")
    jar_dir = Path(os.environ.get("ICLOUD_IMPORT_JAR_DIR") or state / "pyicloud")
    marker = state / "reminders-auth-required"
    dry = "--dry-run" in argv
    if not dry:
        jar_dir.mkdir(parents=True, exist_ok=True)
        os.chmod(jar_dir, 0o700)
    # same filesystem as jar_dir so os.replace is atomic
    # ponytail: blocking LOCK_EX; Rust's 120s timeout + kill_on_drop bounds the wait
    with contextlib.nullcontext() if dry else open(state / "lock", "w") as lk, \
            tempfile.TemporaryDirectory(dir=None if dry else jar_dir.parent) as td:
        if lk:
            fcntl.flock(lk, fcntl.LOCK_EX)
        work = Path(td)
        os.chmod(td, 0o700)
        has_tok = seed_session(work, acct, cookies, jar_dir if jar_dir.exists() else None)
        build_jar(cookies, work, acct)
        ok, reason, n = validate(work, acct)
        if ok and not dry:
            for f in work.iterdir():
                os.replace(f, jar_dir / f.name)
        names = sorted({c["name"] for c in cookies})
        print(json.dumps({
            "ok": ok, "kind": None if ok else "auth", "msg": None if ok else reason,
            "cookies": names, "token_expiry": token_expiry(cookies), "session_token_cookie": has_tok,
            "lists": n, "dry_run": dry,
        }))
        if not ok:
            return 2
    if not dry:
        marker.unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
