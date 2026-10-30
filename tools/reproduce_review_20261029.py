#!/usr/bin/env python3
"""Reproduce observed defects at a frozen revision; passing probes CONFIRM defects."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    root = Path(__file__).resolve().parents[1]
    expected = "2be564fbea6db8ad538829ab8f4bf3521ecf22d2"
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    if revision != expected:
        raise SystemExit("This probe is frozen to " + expected + "; use a checkout of that revision.")
    if subprocess.run(["git", "diff", "--quiet", "HEAD", "--", "src", "assets", "Cargo.toml",
                       "Cargo.lock", "docs/configuration.md"], cwd=root).returncode:
        raise SystemExit("Tracked inputs differ from the frozen revision; refusing to overwrite evidence.")
    if os.name != "posix" or not Path("/usr/bin/true").exists():
        raise SystemExit("These probes require a Unix host with git, sh, head, sleep, and /usr/bin/true.")
    output = Path(tempfile.mkdtemp(prefix="congminal-review-20261029-"))
    source = output / "source"
    source.mkdir()
    for name in ("src", "assets"):
        shutil.copytree(root / name, source / name)
    for name in ("Cargo.toml", "Cargo.lock"):
        shutil.copy2(root / name, source / name)
    (source / "docs").mkdir()
    shutil.copy2(root / "docs/configuration.md", source / "docs/configuration.md")
    evidence = root / "docs/evaluations/review-20261029"
    for target, fixture in (("src/git/git_tests.rs", "git-probes.rs"),
                            ("src/monitor/remote.rs", "monitor-probe.rs")):
        with (source / target).open("a") as stream:
            stream.write("\n" + (evidence / fixture).read_text())
    command = ["cargo", "test", "--locked", "--offline", "--manifest-path", str(source / "Cargo.toml"),
               "--bin", "congminal", "reviewcheck_", "--", "--test-threads=1", "--nocapture"]
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(output / "target")
    print("These probes assert observed defects. A pass is NOT a correctness certificate.", flush=True)
    print("Source and full log retained at:", output, flush=True)
    with (output / "probes.log").open("w") as stream:
        result = subprocess.run(command, cwd=root, env=env, stdout=stream, stderr=subprocess.STDOUT)
    (output / "run.json").write_text(json.dumps({"revision": revision, "command": command,
        "returncode": result.returncode, "passing_means_defect_confirmed": True}, indent=2) + "\n")
    print((output / "probes.log").read_text())
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
