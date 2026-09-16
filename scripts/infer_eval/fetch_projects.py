"""Clone the training projects, at their newest commit or at a recorded one.

By default every project is fetched at the head of its default branch, so a
retrain sees the code as it is today -- that is the point of retraining. What
makes the run reproducible is not a pin here but the record afterwards: the
resolved commit of every project is written to `fetched.json` beside the
checkouts, and `export_models.py` copies it into the model manifest. Replaying
an old run is then `--pinned semantic/models/MANIFEST.json`, which fetches
exactly the commits those weights were built from.

The project list is `projects.txt`: one `name url` per line. Adding a line adds
training data from the next fetch on.

usage: fetch_projects.py [--projects FILE] [--pinned MANIFEST] [--out DIR]
                         [--jobs N] [--only A,B]
"""
import argparse
import datetime
import json
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DATA = os.path.join(REPO, "target", "inference-eval")
PROJECTS = os.path.join(os.path.dirname(__file__), "projects.txt")
MANIFEST = os.path.join(REPO, "semantic", "models", "MANIFEST.json")


def git(args, cwd, check=True):
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {r.stderr.strip()[:300]}")
    return r


def head_of(path):
    if not os.path.isdir(os.path.join(path, ".git")):
        return None
    r = git(["rev-parse", "HEAD"], path, check=False)
    return r.stdout.strip() if r.returncode == 0 else None


def remote_head(url):
    """The commit at the tip of the remote's default branch."""
    r = subprocess.run(["git", "ls-remote", "--symref", url, "HEAD"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"git ls-remote {url}: {r.stderr.strip()[:200]}")
    for line in r.stdout.splitlines():
        if line.endswith("\tHEAD") and not line.startswith("ref:"):
            return line.split("\t")[0]
    raise RuntimeError(f"{url}: no HEAD in ls-remote output")


def read_projects(path):
    out = {}
    with open(path) as fh:
        for n, line in enumerate(fh, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            parts = line.split()
            if len(parts) != 2:
                raise SystemExit(f"{path}:{n}: expected `name url`, found {line!r}")
            out[parts[0]] = parts[1]
    return out


def fetch(item):
    name, url, commit, out = item
    path = os.path.join(out, name)
    # Without a pin, ask the remote what its default branch points at now. The
    # answer is what gets recorded, so "newest" is never ambiguous after the fact.
    commit = commit or remote_head(url)
    if head_of(path) == commit:
        return name, commit, "present"
    os.makedirs(path, exist_ok=True)
    if not os.path.isdir(os.path.join(path, ".git")):
        git(["init", "-q"], path)
        git(["remote", "add", "origin", url], path)
    else:
        git(["remote", "set-url", "origin", url], path)
    try:
        # One commit is a fraction of the history, and no tags are needed.
        git(["fetch", "-q", "--depth", "1", "--no-tags", "origin", commit], path)
        git(["checkout", "-q", "--detach", "FETCH_HEAD"], path)
    except RuntimeError:
        # A server that refuses a fetch by object id: take the history and
        # check the commit out of it.
        git(["fetch", "-q", "--no-tags", "origin"], path)
        git(["checkout", "-q", "--detach", commit], path)
    got = head_of(path)
    if got != commit:
        raise RuntimeError(f"{name}: checked out {got}, expected {commit}")
    return name, commit, "fetched"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--projects", default=PROJECTS, help="name/url list")
    ap.add_argument("--pinned", nargs="?", const=MANIFEST, metavar="MANIFEST",
                    help="reproduce a past run: fetch the commits this manifest records")
    ap.add_argument("--out", default=os.path.join(DATA, "ext"))
    ap.add_argument("--jobs", type=int, default=min(8, os.cpu_count() or 4))
    ap.add_argument("--only", help="comma-separated project names")
    a = ap.parse_args()

    urls = read_projects(a.projects)
    pins = {}
    if a.pinned:
        recorded = json.load(open(a.pinned)).get("training_data") or {}
        if not recorded:
            print(f"{a.pinned} records no training_data", file=sys.stderr)
            return 1
        pins = {n: s["commit"] for n, s in recorded.items() if s.get("commit")}
        # A pinned replay is that manifest's projects, whatever projects.txt
        # has grown since; its URLs win too, in case one moved.
        urls = {n: s.get("url") or urls.get(n) for n, s in recorded.items()}
    if a.only:
        wanted = {n.strip() for n in a.only.split(",")}
        missing = wanted - set(urls)
        if missing:
            print(f"not listed: {', '.join(sorted(missing))}", file=sys.stderr)
            return 1
        urls = {k: v for k, v in urls.items() if k in wanted}

    os.makedirs(a.out, exist_ok=True)
    items = [(n, urls[n], pins.get(n), a.out) for n in sorted(urls)]
    where = "pinned commits" if a.pinned else "newest commit on each default branch"
    print(f"{len(items)} projects -> {a.out} ({where})", flush=True)
    resolved, failures = {}, []
    with ThreadPoolExecutor(a.jobs) as ex:
        futures = {ex.submit(fetch, it): it[0] for it in items}
        for done, (fut, name) in enumerate(futures.items(), 1):
            try:
                name, commit, what = fut.result()
                resolved[name] = {"commit": commit, "url": urls[name]}
                print(f"  [{done:2d}/{len(items)}] {name:12s} {what:8s} {commit[:12]}", flush=True)
            except Exception as exc:  # one bad remote must not lose the rest
                failures.append((name, str(exc).replace("\n", " ")[:200]))
                print(f"  [{done:2d}/{len(items)}] {name:12s} FAILED", flush=True)

    if resolved and not a.only:
        # What the next training run reads, and what export_models.py copies
        # into the manifest: the record that makes this fetch repeatable.
        with open(os.path.join(a.out, "fetched.json"), "w") as fh:
            json.dump({
                "fetched": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
                "pinned": bool(a.pinned),
                "projects": dict(sorted(resolved.items())),
            }, fh, indent=1)
            fh.write("\n")

    if failures:
        print("\nfailed:", file=sys.stderr)
        for name, err in failures:
            print(f"  {name}: {err}", file=sys.stderr)
        return 1
    print(f"\nall {len(items)} projects checked out; recorded in {a.out}/fetched.json")
    return 0


if __name__ == "__main__":
    sys.exit(main())
