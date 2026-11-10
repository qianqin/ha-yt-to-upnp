"""Packs the built artefacts folder into the release tarball and writes artefacts.lock.

Called by scripts/build-ha-app.sh --release:

  python3 -I scripts/pack_artefacts.py --version V --mtime EPOCH --remote URL \
      ARTEFACTS_DIR OUT_DIR LOCK_PATH

The tarball is reproducible: the same files give the same bytes, whatever their
mtimes, owners or umask on the release machine (sorted entries, one fixed mtime,
owner 0:0 without names, modes 0755/0644, gzip header without time or name).
"""
import argparse
import gzip
import hashlib
import os
import re
import stat
import sys
import tarfile

ASSET_PREFIX = "ha-yt-to-upnp-artefacts"
ARCH = "amd64"
TOP = "artefacts"

_VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+\Z")
_SHA256 = re.compile(r"[0-9a-f]{64}\Z")
_NAME = r"[A-Za-z0-9][A-Za-z0-9._-]*"
# No user info: a token in the remote URL must never end up in the committed lock.
_REMOTES = [re.compile(p) for p in (
    r"https://github\.com/(%s)/(%s)/?\Z" % (_NAME, _NAME),
    r"git@github\.com:(%s)/(%s)/?\Z" % (_NAME, _NAME),
    r"ssh://git@github\.com/(%s)/(%s)/?\Z" % (_NAME, _NAME),
)]


def github_repo(remote):
    """(owner, repo) of a GitHub remote URL; ValueError for anything else."""
    for pattern in _REMOTES:
        m = pattern.match(remote)
        if m:
            repo = m.group(2)
            repo = repo[:-4] if repo.endswith(".git") else repo
            if repo:
                return m.group(1), repo
    raise ValueError("not a GitHub repository URL without credentials: %r" % remote)


def asset_name(version):
    if not _VERSION.match(version):
        raise ValueError("version must be MAJOR.MINOR.PATCH: %r" % version)
    return "%s-%s-%s.tar.gz" % (ASSET_PREFIX, version, ARCH)


def asset_url(repo, version):
    owner, name = repo
    return "https://github.com/%s/%s/releases/download/v%s/%s" % (owner, name, version, asset_name(version))


def _entries(root):
    """Paths relative to root, sorted, root's own entry first as ''."""
    found = [""]
    for dirpath, dirnames, filenames in os.walk(root):
        rel = os.path.relpath(dirpath, root)
        for name in dirnames + filenames:
            found.append(name if rel == "." else os.path.join(rel, name))
    return sorted(found, key=lambda p: p.split(os.sep))


def _check_link(root, path):
    target = os.readlink(path)
    resolved = os.path.normpath(os.path.join(os.path.dirname(path), target))
    if os.path.isabs(target) or os.path.relpath(resolved, root).split(os.sep)[0] == "..":
        raise ValueError("link %s points out of the artefacts folder: %s" % (path, target))
    return target


def pack(root, out_path, mtime):
    """Writes the tarball of root (as artefacts/) to out_path; returns its sha256."""
    root = os.path.abspath(root)
    with open(out_path, "wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for rel in _entries(root):
                    path = os.path.join(root, rel)
                    st = os.lstat(path)
                    info = tarfile.TarInfo("/".join([TOP] + (rel.split(os.sep) if rel else [])))
                    info.mtime, info.uid, info.gid, info.uname, info.gname = mtime, 0, 0, "", ""
                    if stat.S_ISLNK(st.st_mode):
                        info.type, info.linkname, info.mode = tarfile.SYMTYPE, _check_link(root, path), 0o777
                        tar.addfile(info)
                    elif stat.S_ISDIR(st.st_mode):
                        info.type, info.mode = tarfile.DIRTYPE, 0o755
                        tar.addfile(info)
                    elif stat.S_ISREG(st.st_mode):
                        info.size = st.st_size
                        info.mode = 0o755 if st.st_mode & 0o111 else 0o644
                        with open(path, "rb") as f:
                            tar.addfile(info, f)
                    else:
                        raise ValueError("not a regular file, directory or link: %s" % path)
    digest = hashlib.sha256()
    with open(out_path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def write_lock(path, version, url, sha256):
    asset_name(version)
    if not _SHA256.match(sha256):
        raise ValueError("sha256 must be 64 lowercase hex digits: %r" % sha256)
    with open(path, "w") as f:
        f.write("# Written by scripts/build-ha-app.sh --release; the Dockerfile downloads\n"
                "# the url and checks the sha256 unless artefacts/ is in the build context.\n"
                "version: %s\nurl: %s\nsha256: %s\n" % (version, url, sha256))


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--mtime", required=True, type=int)
    parser.add_argument("--remote", required=True)
    parser.add_argument("artefacts_dir")
    parser.add_argument("out_dir")
    parser.add_argument("lock_path")
    args = parser.parse_args(argv)
    url = asset_url(github_repo(args.remote), args.version)
    os.makedirs(args.out_dir, exist_ok=True)
    tarball = os.path.join(args.out_dir, asset_name(args.version))
    digest = pack(args.artefacts_dir, tarball, args.mtime)
    write_lock(args.lock_path, args.version, url, digest)
    print("tarball: %s\nsha256: %s\nurl: %s" % (tarball, digest, url))


if __name__ == "__main__":
    main(sys.argv[1:])
