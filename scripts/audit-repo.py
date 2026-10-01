#!/usr/bin/env python3
"""Audit EVERY reachable git object (all commits; text and binary files) for secrets and account
information before anything is published.

The matched text is never printed, only the category and the file path, so the audit itself
cannot leak anything.

Personal words (e-mail addresses, company names, account / organisation ids ...) come from
  .git/info/blocked-words.txt   (one regex per line, never committed: put YOUR words here)
  .githooks/blocked-words.txt   (generic words, committed)

Run by hand:  python scripts/audit-repo.py        (also run by .githooks/pre-push)
Exit code 0 = clean, 1 = problems found.
"""
import re
import subprocess
import sys
from pathlib import Path

# Not an account id: the public OAuth client id of Claude Code (it is a constant in the source).
PUBLIC_UUIDS = {"9d1c250a-e61b-44d9-88ed-5944d1962f5e"}
NOREPLY = rb"[0-9]+\+[A-Za-z0-9-]+@users\.noreply\.github\.com"
ALLOWED_EMAIL = re.compile(
    rb"^(noreply@anthropic\.com|[A-Za-z0-9._%+-]+@example\.(com|org)|" + NOREPLY + rb")$"
)

SECRET_SHAPES = {
    "anthropic key/token": re.compile(rb"sk-ant-[A-Za-z0-9_-]{20,}"),
    "google access token": re.compile(rb"ya29\.[A-Za-z0-9_-]{20,}"),
    "google refresh token": re.compile(rb"1//0[A-Za-z0-9_-]{20,}"),
    "google client secret": re.compile(rb"GOCSPX-[A-Za-z0-9_-]{8,}"),
    "jwt": re.compile(rb"eyJ[A-Za-z0-9_-]{15,}\.[A-Za-z0-9_-]{15,}\.[A-Za-z0-9_-]{5,}"),
    "github token": re.compile(rb"gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}"),
    "aws key": re.compile(rb"AKIA[0-9A-Z]{12,}"),
    "private key block": re.compile(rb"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    "token assignment": re.compile(
        rb"(?i)(access|refresh|api)[_-]?(token|key)\"?\s*[:=]\s*\"[A-Za-z0-9_./+-]{24,}\""
    ),
}
CREDENTIAL_FILE_NAMES = {
    ".credentials.json", "auth.json", "oauth_creds.json", "oauth-account.json",
    "cache.json", "settings.json", ".env",
}
UUID = re.compile(rb"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
EMAIL = re.compile(rb"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+")
WINPATH = re.compile(rb"(?i)[A-Z]:\\{1,2}Users\\{1,2}[A-Za-z0-9._ -]+")


def git(*args, stdin=None):
    return subprocess.run(["git", *args], input=stdin, capture_output=True, check=True).stdout


def load_blocklist():
    words = []
    for name in (".git/info/blocked-words.txt", ".githooks/blocked-words.txt"):
        f = Path(name)
        if not f.is_file():
            continue
        for line in f.read_text(encoding="utf-8", errors="replace").splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                try:
                    words.append(re.compile(line.encode(), re.I))
                except re.error:
                    print(f"  (ignored invalid regex in {name})")
    return words


def reachable_objects():
    """sha -> set(paths) for every object reachable from any ref."""
    out = {}
    for line in git("rev-list", "--objects", "--all").decode("utf-8", "replace").splitlines():
        sha, _, path = line.partition(" ")
        out.setdefault(sha, set())
        if path:
            out[sha].add(path)
    return out


def read_objects(shas):
    """Yield (sha, type, bytes) using one `git cat-file --batch` process."""
    p = subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    for sha in shas:
        p.stdin.write(sha.encode() + b"\n")
        p.stdin.flush()
        header = p.stdout.readline().split()
        size = int(header[2])
        data = p.stdout.read(size)
        p.stdout.read(1)  # trailing newline
        yield sha, header[1].decode(), data
    p.stdin.close()
    p.wait()


def main():
    blocklist = load_blocklist()
    objects = reachable_objects()
    problems = set()
    uuids, emails, winpaths = {}, {}, {}
    scanned = 0

    for sha, typ, data in read_objects(list(objects)):
        scanned += 1
        where = ", ".join(sorted(objects[sha])) or f"{typ} {sha[:8]}"
        text = data
        if typ == "commit":  # the anonymised GitHub noreply author address is expected
            text = re.sub(NOREPLY, b"", text)
        for rx in blocklist:
            if rx.search(text):
                problems.add(("personal word / account id (from your blocklist)", where))
        for name, rx in SECRET_SHAPES.items():
            if rx.search(data):
                problems.add((f"secret shape: {name}", where))
        if typ == "blob":
            for path in objects[sha]:
                if Path(path).name in CREDENTIAL_FILE_NAMES or Path(path).suffix in {".pem", ".key", ".p12", ".pfx"}:
                    problems.add(("credential-like file name", path))
        for m in UUID.findall(data):
            if m.decode().lower() not in PUBLIC_UUIDS:
                uuids.setdefault(m.decode().lower(), set()).add(where)
        for m in EMAIL.findall(data):
            if not ALLOWED_EMAIL.match(m):
                emails.setdefault(m.decode(errors="replace"), set()).add(where)
        for m in WINPATH.findall(data):
            winpaths.setdefault(m.decode(errors="replace"), set()).add(where)

    for u, w in uuids.items():
        problems.add(("uuid that may be an account / organisation id", ", ".join(sorted(w))))
    for e, w in emails.items():
        problems.add(("e-mail address not on the allow-list", ", ".join(sorted(w))))
    for pth, w in winpaths.items():
        problems.add(("local Windows user path", ", ".join(sorted(w))))

    print(f"audit: {scanned} objects scanned, {len(blocklist)} blocklist pattern(s) in use")
    if problems:
        for category, where in sorted(problems):
            print(f"  FOUND  {category}  in  {where}")
        print("AUDIT RESULT: PROBLEMS FOUND (nothing is printed from the files themselves)")
        return 1
    print("AUDIT RESULT: CLEAN")
    return 0


if __name__ == "__main__":
    sys.exit(main())
