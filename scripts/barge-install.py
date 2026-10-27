"""Build `barge` from a pushed-main pin and install it at ~/.local/bin/barge.

Initial mode refuses any `barge` on PATH and any existing destination entry.
`--replace` swaps only the regular, user-owned binary whose SHA-256 matches
the newest install receipt. Each run writes a new receipt; earlier receipts
are kept for rollback. `--check` stops after the preflight and builds nothing.
Effigy does not forward selector arguments, so `BARGE_INSTALL_PIN` also names
the pin.

Every build carries the authenticated source pin and a fresh instance. The
built binary must report that identity before it can be installed.
"""
import argparse
import datetime
import hashlib
import json
import os
import re
import secrets
import shutil
import stat
import subprocess
import sys
from pathlib import Path

HOME = Path(os.environ["HOME"])
DESTINATION = HOME / ".local/bin/barge"
RECEIPTS = HOME / ".local/state/barge/install"
BUILD = ["oxide", "build", "--release", "--locked", "--offline", "-p", "barge-cli"]
IDENTITY_ARGUMENT = "__barge-build-identity"
BUILD_PIN_VARIABLE = "BARGE_BUILD_PIN"
BUILD_INSTANCE_VARIABLE = "BARGE_BUILD_INSTANCE"
REPOSITORY = Path(__file__).resolve().parent.parent
REMOTE_MAIN = "refs/heads/main"


class Refusal(Exception):
    pass


def git(*args):
    return subprocess.run(
        ["git", "-C", str(REPOSITORY), *args], check=True, capture_output=True, text=True
    ).stdout.strip()


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def authenticate_pin(requested):
    """Return (pin, tree, remote tip); the pin must be reachable from origin main."""
    if not requested or not re.fullmatch(r"[0-9a-f]{40}", requested):
        raise Refusal("BARGE_INSTALL_PIN must be a 40-character lowercase commit")
    listing = git("ls-remote", "origin", REMOTE_MAIN).split()
    if len(listing) != 2 or listing[1] != REMOTE_MAIN:
        raise Refusal(f"origin {REMOTE_MAIN} not found")
    tip = listing[0]
    git("fetch", "--quiet", "origin", REMOTE_MAIN)
    if git("rev-parse", "FETCH_HEAD") != tip:
        raise Refusal("fetched main does not match ls-remote; retry")
    pin = git("rev-parse", "--verify", f"{requested or tip}^{{commit}}")
    if subprocess.run(
        ["git", "-C", str(REPOSITORY), "merge-base", "--is-ancestor", pin, tip]
    ).returncode != 0:
        raise Refusal(f"{pin} is not on pushed origin main ({tip})")
    return pin, git("rev-parse", f"{pin}^{{tree}}"), tip


def path_entries():
    """Every `barge` entry in a PATH directory, links included."""
    found = []
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        candidate = Path(os.path.abspath(Path(directory or ".") / "barge"))
        if os.path.lexists(candidate) and candidate not in found:
            found.append(candidate)
    return found


def check_directory_chain(directory, allow_missing=False):
    """Refuse symlinks, foreign ownership and writable ancestors below HOME."""
    parts = directory.relative_to(HOME).parts
    for count in range(len(parts) + 1):
        item = HOME.joinpath(*parts[:count])
        try:
            info = os.lstat(item)
        except FileNotFoundError:
            if allow_missing:
                return
            raise
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid()
                or (count and info.st_mode & 0o022)):
            raise Refusal(f"{item} is not a private owned real directory")


def newest_receipt():
    check_directory_chain(RECEIPTS, allow_missing=True)
    if not RECEIPTS.exists():
        return None
    receipts = sorted(RECEIPTS.glob("*.json"))
    if not receipts:
        return None
    receipt = receipts[-1]
    info = os.lstat(receipt)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise Refusal(f"unsafe receipt {receipt}")
    try:
        data = json.loads(receipt.read_text())
        digest = data["installed"]["sha256"]
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError("invalid hash")
    except (ValueError, KeyError, TypeError) as error:
        raise Refusal(f"invalid receipt {receipt}: {error}") from error
    return receipt, data


def open_destination_directory():
    """Open the destination's parent after refusing any link or foreign directory.

    Every component from HOME down must be a real directory owned by this
    user, and none below HOME may be group or world writable.
    """
    check_directory_chain(DESTINATION.parent)
    descriptor = os.open(DESTINATION.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    opened, checked = os.fstat(descriptor), os.lstat(DESTINATION.parent)
    if (opened.st_dev, opened.st_ino) != (checked.st_dev, checked.st_ino):
        os.close(descriptor)
        raise Refusal(f"{DESTINATION.parent} changed while it was checked")
    return descriptor


def identity(directory, name):
    """(device, inode, SHA-256) of a regular user-owned entry, or None if absent.

    Links, other file types and foreign owners refuse.
    """
    try:
        info = os.stat(name, dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        raise Refusal(f"{DESTINATION.parent / name} is not a regular file owned by this user")
    descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory)
    with os.fdopen(descriptor, "rb") as file:
        opened = os.fstat(file.fileno())
        if (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
            raise Refusal(f"{DESTINATION.parent / name} changed while it was read")
        digest = hashlib.sha256()
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    return info.st_dev, info.st_ino, digest.hexdigest()


def preflight(replace):
    """Return (replaced receipt, expected destination identity), or Nones in initial mode."""
    check_directory_chain(RECEIPTS, allow_missing=True)
    entries = path_entries()
    if not replace:
        if entries:
            raise Refusal("barge already on PATH: " + ", ".join(map(str, entries)))
        if os.path.lexists(DESTINATION):
            raise Refusal(f"{DESTINATION} already exists")
        os.close(open_destination_directory())
        return None, None
    others = [entry for entry in entries if entry != DESTINATION]
    if others:
        raise Refusal("another barge on PATH: " + ", ".join(map(str, others)))
    previous = newest_receipt()
    if previous is None:
        raise Refusal(f"no install receipt under {RECEIPTS}; use initial mode")
    directory = open_destination_directory()
    try:
        expected = identity(directory, DESTINATION.name)
    finally:
        os.close(directory)
    if expected is None or expected[2] != previous[1]["installed"]["sha256"]:
        raise Refusal(f"{DESTINATION} does not match receipt {previous[0].name}")
    return previous, expected


def new_instance():
    """A fresh 128-bit lowercase-hex nonce for exactly this build."""
    return secrets.token_hex(16)


def build_environment(staging, pin, instance):
    """The build's environment: private target, authenticated pin, fresh instance.

    Inherited `CARGO_TARGET_*` names are dropped so the build cannot write to
    a caller's storage. The stamp names are read by `option_env!`, which
    Cargo tracks, so a changed stamp recompiles the binary.
    """
    environment = {k: v for k, v in os.environ.items()
                   if not k.startswith("CARGO_TARGET_") and k != "CARGO_BUILD_BUILD_DIR"
                   and not k.startswith("OXIDE_")}
    for name in ("TMPDIR", "TMP", "TEMP"):
        environment[name] = str(staging)
    environment["OXIDE_POOL"] = str(staging / "p")
    environment["OXIDE_CONFIG"] = str(staging / "o.toml")
    (staging / "o.toml").write_text("")
    environment[BUILD_PIN_VARIABLE] = pin
    environment[BUILD_INSTANCE_VARIABLE] = instance
    return environment


def build(pin, instance, staging):
    source = staging / "s"
    source.mkdir()
    (source / "target").mkdir()
    archive = subprocess.Popen(
        ["git", "-C", str(REPOSITORY), "archive", pin], stdout=subprocess.PIPE
    )
    subprocess.run(["tar", "-x", "-C", str(source)], stdin=archive.stdout, check=True)
    if archive.wait() != 0:
        raise Refusal("git archive failed")
    environment = build_environment(staging, pin, instance)
    versions = {
        tool: subprocess.run(
            [tool, "-vV" if tool == "rustc" else "-V"],
            cwd=source, env=environment, check=True, capture_output=True, text=True,
        ).stdout.strip()
        for tool in ("rustc", "cargo")
    }
    subprocess.run(BUILD, cwd=source, env=environment, check=True)
    return source / "target/release/barge", versions


def build_identity(built, pin, instance):
    """The stamp the built binary actually carries, or a refusal.

    This runs the built binary's own effect-free probe, so a build that missed
    its stamp or kept a stale one can never be installed with a receipt that
    claims it.
    """
    try:
        result = subprocess.run(
            [str(built), IDENTITY_ARGUMENT], check=True, capture_output=True, text=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise Refusal(f"the built barge cannot report its build identity: {error}")
    try:
        reported = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise Refusal(f"the built barge printed no build identity JSON: {error}")
    if not isinstance(reported, dict):
        raise Refusal("the built barge printed no build identity object")
    for name, expected in (("build_pin", pin), ("build_instance", instance)):
        if reported.get(name) != expected:
            raise Refusal(f"the built barge's {name} does not match this build")
    return reported


def write_receipt(receipt, started, pin):
    """Write one new receipt owner-only; earlier receipts are left untouched."""
    check_directory_chain(RECEIPTS, allow_missing=True)
    RECEIPTS.mkdir(parents=True, exist_ok=True, mode=0o700)
    check_directory_chain(RECEIPTS)
    info = os.lstat(RECEIPTS)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise Refusal(f"{RECEIPTS} is not a real directory owned by this user")
    name = f"{started.strftime('%Y%m%dT%H%M%S%fZ')}-{pin[:12]}-{receipt['build_instance']}.json"
    descriptor = os.open(
        RECEIPTS / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600
    )
    with os.fdopen(descriptor, "w") as file:
        json.dump(receipt, file, indent=2)
        file.write("\n")
    return RECEIPTS / name


def install(built, expected):
    """Copy through a private sibling, then link (initial) or rename (replace).

    The destination is rechecked through the opened parent directly before the
    link or rename: absent for link, the preflight's exact identity for rename.
    """
    built_hash = sha256(built)
    directory = open_destination_directory()
    temporary = f".barge.install.{os.getpid()}.{secrets.token_hex(8)}"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    try:
        with os.fdopen(os.open(temporary, flags, 0o700, dir_fd=directory), "wb") as output:
            with open(built, "rb") as source:
                shutil.copyfileobj(source, output)
            output.flush()
            os.fchmod(output.fileno(), 0o755)
            os.fsync(output.fileno())
        staged = identity(directory, temporary)
        if staged[2] != built_hash:
            raise Refusal("staged copy hash differs from the build output")
        if identity(directory, DESTINATION.name) != expected:
            raise Refusal(f"{DESTINATION} changed during the build")
        if expected is None:
            # link() refuses an existing destination: no-clobber.
            os.link(temporary, DESTINATION.name, src_dir_fd=directory, dst_dir_fd=directory)
        else:
            os.rename(temporary, DESTINATION.name, src_dir_fd=directory, dst_dir_fd=directory)
        if identity(directory, DESTINATION.name) != staged:
            raise Refusal(f"{DESTINATION} is not the staged file after installation")
        info = os.stat(DESTINATION.name, dir_fd=directory, follow_symlinks=False)
    finally:
        try:
            os.unlink(temporary, dir_fd=directory)
        except FileNotFoundError:
            pass
        os.close(directory)
    installed = {
        "path": str(DESTINATION),
        "owner_uid": info.st_uid,
        "mode": oct(stat.S_IMODE(info.st_mode)),
        "size": info.st_size,
        "mtime": datetime.datetime.fromtimestamp(info.st_mtime, datetime.timezone.utc).isoformat(),
        "sha256": staged[2],
    }
    return built_hash, installed


def cleanup_staging(staging):
    """Remove only the installer-owned entries, never the caller's directory."""
    for name in ("s", "p"):
        item = staging / name
        if item.exists():
            shutil.rmtree(item)
    config = staging / "o.toml"
    if config.exists():
        config.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--pin",
        default=os.environ.get("BARGE_INSTALL_PIN") or None,
        help="40-hex commit on origin main (default: BARGE_INSTALL_PIN)",
    )
    parser.add_argument("--replace", action="store_true", help="replace the receipt-matching binary")
    parser.add_argument("--check", action="store_true", help="preflight only; build nothing")
    arguments = parser.parse_args()
    started = datetime.datetime.now(datetime.timezone.utc)
    staging = None
    try:
        pin, tree, tip = authenticate_pin(arguments.pin)
        previous, expected = preflight(arguments.replace)
        print(f"pin {pin} tree {tree} (origin main {tip})")
        print(f"destination {DESTINATION}: " + ("absent" if previous is None else f"matches {previous[0].name}"))
        if arguments.check:
            print("check only: nothing built or installed")
            return 0
        scratch = Path(os.environ["BARGE_INSTALL_SCRATCH"])
        info = os.lstat(scratch)
        if (not scratch.is_absolute() or scratch == Path(scratch.anchor)
                or scratch.resolve() != scratch or os.path.ismount(scratch)
                or scratch == HOME or HOME in scratch.parents
                or not stat.S_ISDIR(info.st_mode)
                or info.st_uid != os.getuid() or info.st_mode & 0o022):
            raise Refusal("BARGE_INSTALL_SCRATCH must be an absolute, private, owned real directory")
        if any(scratch.iterdir()):
            raise Refusal("BARGE_INSTALL_SCRATCH must be empty and exclusively owned")
        socket_bound = scratch / "p/payload/slots/s-12345678/quick-1/scratch"
        if len(os.fsencode(socket_bound)) > 91:
            raise Refusal("BARGE_INSTALL_SCRATCH is too long for Oxide's socket bound")
        staging = scratch
        (staging / "p").mkdir()
        print(f"staging {staging}")
        instance = new_instance()
        built, versions = build(pin, instance, staging)
        build_identity(built, pin, instance)
        current, current_identity = preflight(arguments.replace)
        if current != previous or current_identity != expected:
            raise Refusal("destination or receipt changed during the build")
        built_hash, installed = install(built, expected)
    except (Refusal, subprocess.CalledProcessError, OSError, KeyError, ValueError) as error:
        print(f"barge:install refused: {error}", file=sys.stderr)
        if staging is not None:
            cleanup_staging(staging)
        return 1
    receipt = {
        "mode": "initial" if previous is None else "replace",
        "started": started.isoformat(),
        "finished": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "pin": pin,
        "tree": tree,
        "origin_main": tip,
        "build": " ".join(BUILD),
        "toolchain": versions,
        "built_sha256": built_hash,
        "build_instance": instance,
        "installed": installed,
        "prior": "absent" if previous is None else {
            "receipt": previous[0].name, "sha256": previous[1]["installed"]["sha256"],
        },
    }
    try:
        path = write_receipt(receipt, started, pin)
    except (Refusal, OSError) as error:
        print(f"barge:install installed but cannot write a receipt: {error}", file=sys.stderr)
        print(json.dumps(receipt, indent=2))
        cleanup_staging(staging)
        return 1
    cleanup_staging(staging)
    print(json.dumps(receipt, indent=2))
    print(f"receipt {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
