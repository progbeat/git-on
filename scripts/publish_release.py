#!/usr/bin/env python3
"""Publish tested artifacts for exactly this master SHA; safely retry partial uploads."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def gh(*args, raw=False, data=None):
    result = subprocess.run(["gh", *args], input=None if data is None else json.dumps(data),
                            text=True, check=True, stdout=subprocess.PIPE)
    return result.stdout if raw else json.loads(result.stdout)


def api(path, data=None):
    args = ["api", path]
    if data is not None:
        args += ["--method", "POST", "--input", "-"]
    return gh(*args, data=data)


def publish():
    repo = os.environ["GITHUB_REPOSITORY"]
    sha = os.environ["GITHUB_SHA"]
    if os.environ.get("GITHUB_EVENT_NAME") != "push" or os.environ.get("GITHUB_REF") != "refs/heads/master":
        raise RuntimeError("publishing is restricted to a master push")
    manifest = json.loads(Path("dist/release.json").read_text())
    if manifest["commit"] != sha:
        raise RuntimeError("artifact source SHA differs from the tested master SHA")
    tag = "v" + manifest["version"]
    # Never use 'latest master' as the target: overlapping merges may finish out of order.
    refs = api(f"repos/{repo}/git/matching-refs/tags/{tag}")
    refs = [r for r in refs if r["ref"] == f"refs/tags/{tag}"]
    if refs:
        if refs[0]["object"]["type"] != "commit" or refs[0]["object"]["sha"] != sha:
            raise RuntimeError("version tag already points to another object; refusing to move it")
    else:
        api(f"repos/{repo}/git/refs", {"ref": f"refs/tags/{tag}", "sha": sha})
    # Include drafts so a failed earlier upload can be completed, not recreated.
    releases = gh("api", "--paginate", "--slurp", f"repos/{repo}/releases?per_page=100")
    matches = [r for page in releases for r in page if r["tag_name"] == tag]
    if len(matches) > 1:
        raise RuntimeError("multiple releases for one version")
    body = Path("release-body.md").read_text().strip() + f"\n\nSource commit: {sha}\n"
    if matches:
        release = matches[0]
        if release["body"].strip() != body.strip():
            raise RuntimeError("existing release notes differ; refusing to rewrite a release")
    else:
        release = api(f"repos/{repo}/releases", {"tag_name": tag, "target_commitish": sha,
                      "name": tag, "body": body, "draft": True, "prerelease": False})
    files = sorted(p for p in Path("dist").iterdir() if p.is_file())
    expected = {p.name for p in files}
    assets = {a["name"]: a for a in api(f"repos/{repo}/releases/{release['id']}/assets?per_page=100")}
    if set(assets) - expected:
        raise RuntimeError("unexpected existing release assets")
    for path in files:
        if path.name in assets:
            with tempfile.TemporaryDirectory() as directory:
                gh("release", "download", tag, "--repo", repo, "--pattern", path.name,
                   "--dir", directory, raw=True)
                received = Path(directory, path.name).read_bytes()
                if hashlib.sha256(received).digest() != hashlib.sha256(path.read_bytes()).digest():
                    raise RuntimeError(f"existing asset differs: {path.name}")
        elif release["draft"]:
            gh("release", "upload", tag, str(path), "--repo", repo, raw=True)
        else:
            raise RuntimeError("published release is missing an asset; refusing mutation")
    if release["draft"]:
        # 'legacy' chooses latest using version/date semantics, not job completion order.
        gh("api", f"repos/{repo}/releases/{release['id']}", "--method", "PATCH", "--input", "-",
           data={"draft": False, "make_latest": "legacy"})
    print(f"Verified release {tag} at {sha}")


if __name__ == "__main__":
    publish()
