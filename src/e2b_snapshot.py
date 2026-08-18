#!/usr/bin/env python3
"""Root-only, no-follow evidence extraction for an immutable E2B rootfs mount."""

import hashlib
import json
import os
import stat
import sys
import tarfile


def fail(message):
    raise RuntimeError(message)


def safe_text(value, label):
    if any(0xD800 <= ord(character) <= 0xDFFF for character in value):
        fail(f"{label} is not valid UTF-8")
    return value


def relative_path(value, allow_root=False):
    if not isinstance(value, str):
        fail("path must be a string")
    safe_text(value, "path")
    value = value.lstrip("/")
    if not value and allow_root:
        return ""
    parts = value.split("/")
    if not value or any(part in ("", ".", "..") for part in parts):
        fail(f"unsafe path {value!r}")
    return value


def same_node(left, right):
    return (
        left.st_dev == right.st_dev
        and left.st_ino == right.st_ino
        and left.st_mode == right.st_mode
        and left.st_size == right.st_size
    )


def hash_regular(parent_fd, name, expected):
    descriptor = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW, dir_fd=parent_fd)
    try:
        actual = os.fstat(descriptor)
        if not stat.S_ISREG(actual.st_mode) or not same_node(expected, actual):
            fail(f"filesystem entry changed while hashing {name!r}")
        digest = hashlib.sha256()
        size = 0
        while True:
            block = os.read(descriptor, 1024 * 1024)
            if not block:
                break
            digest.update(block)
            size += len(block)
        if size != expected.st_size:
            fail(f"filesystem entry changed size while hashing {name!r}")
        return "sha256:" + digest.hexdigest()
    finally:
        os.close(descriptor)


def scan_directory(directory_fd, prefix, entries):
    with os.scandir(directory_fd) as iterator:
        children = sorted(iterator, key=lambda item: item.name)
    for child in children:
        name = safe_text(child.name, "filesystem name")
        if name in ("", ".", "..") or "/" in name:
            fail(f"unsafe filesystem name {name!r}")
        path = f"{prefix}/{name}" if prefix else name
        metadata = child.stat(follow_symlinks=False)
        mode = stat.S_IMODE(metadata.st_mode)
        if stat.S_ISREG(metadata.st_mode):
            entries.append(
                {
                    "path": path,
                    "type": "file",
                    "mode": mode,
                    "size": metadata.st_size,
                    "digest": hash_regular(directory_fd, name, metadata),
                }
            )
        elif stat.S_ISDIR(metadata.st_mode):
            entries.append({"path": path, "type": "directory", "mode": mode})
            child_fd = os.open(
                name,
                os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW,
                dir_fd=directory_fd,
            )
            try:
                actual = os.fstat(child_fd)
                if not same_node(metadata, actual):
                    fail(f"filesystem directory changed while scanning {path!r}")
                scan_directory(child_fd, path, entries)
            finally:
                os.close(child_fd)
        elif stat.S_ISLNK(metadata.st_mode):
            target = safe_text(os.readlink(name, dir_fd=directory_fd), "symlink target")
            entries.append(
                {
                    "path": path,
                    "type": "symlink",
                    "mode": mode,
                    "link_target": target,
                }
            )
        else:
            fail(f"unsupported filesystem entry type at /{path}")


def open_parent(root_fd, path):
    parts = relative_path(path).split("/")
    descriptor = os.dup(root_fd)
    try:
        for component in parts[:-1]:
            next_descriptor = os.open(
                component,
                os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW,
                dir_fd=descriptor,
            )
            os.close(descriptor)
            descriptor = next_descriptor
        return descriptor, parts[-1]
    except Exception:
        os.close(descriptor)
        raise


def stable_regular(parent_fd, name):
    before = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    if not stat.S_ISREG(before.st_mode):
        fail(f"requested content /{name} is not a regular file")
    descriptor = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW, dir_fd=parent_fd)
    actual = os.fstat(descriptor)
    if not same_node(before, actual):
        os.close(descriptor)
        fail(f"requested content changed while opening {name!r}")
    return descriptor, actual


def tar_regular_files(root_fd, paths, output):
    with tarfile.open(output, "w", format=tarfile.PAX_FORMAT, dereference=False) as archive:
        for path in sorted(set(relative_path(path) for path in paths)):
            parent_fd, name = open_parent(root_fd, path)
            try:
                descriptor, metadata = stable_regular(parent_fd, name)
                info = tarfile.TarInfo(path)
                info.mode = stat.S_IMODE(metadata.st_mode)
                info.size = metadata.st_size
                info.mtime = 0
                info.uid = 0
                info.gid = 0
                with os.fdopen(descriptor, "rb", closefd=True) as source:
                    archive.addfile(info, source)
            finally:
                os.close(parent_fd)


def add_capture_node(archive, parent_fd, name, archive_name):
    metadata = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    info = tarfile.TarInfo(archive_name)
    info.mode = stat.S_IMODE(metadata.st_mode)
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    if stat.S_ISREG(metadata.st_mode):
        descriptor, actual = stable_regular(parent_fd, name)
        info.size = actual.st_size
        with os.fdopen(descriptor, "rb", closefd=True) as source:
            archive.addfile(info, source)
    elif stat.S_ISDIR(metadata.st_mode):
        info.type = tarfile.DIRTYPE
        info.size = 0
        archive.addfile(info)
        directory_fd = os.open(
            name,
            os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW,
            dir_fd=parent_fd,
        )
        try:
            actual = os.fstat(directory_fd)
            if not same_node(metadata, actual):
                fail(f"capture directory changed while opening {archive_name!r}")
            with os.scandir(directory_fd) as iterator:
                children = sorted(iterator, key=lambda item: item.name)
            for child in children:
                child_name = safe_text(child.name, "capture name")
                if child_name in ("", ".", "..") or "/" in child_name:
                    fail(f"unsafe capture name {child_name!r}")
                add_capture_node(
                    archive,
                    directory_fd,
                    child_name,
                    f"{archive_name}/{child_name}",
                )
        finally:
            os.close(directory_fd)
    elif stat.S_ISLNK(metadata.st_mode):
        info.type = tarfile.SYMTYPE
        info.size = 0
        info.linkname = safe_text(os.readlink(name, dir_fd=parent_fd), "capture link")
        archive.addfile(info)
    else:
        fail(f"unsupported captured filesystem entry type at {archive_name!r}")


def tar_capture(root_fd, guest_path, output):
    path = relative_path(guest_path, allow_root=True)
    with tarfile.open(output, "w", format=tarfile.PAX_FORMAT, dereference=False) as archive:
        if path:
            parent_fd, name = open_parent(root_fd, path)
            try:
                add_capture_node(archive, parent_fd, name, path)
            finally:
                os.close(parent_fd)
        else:
            with os.scandir(root_fd) as iterator:
                children = sorted(iterator, key=lambda item: item.name)
            for child in children:
                name = safe_text(child.name, "capture name")
                add_capture_node(archive, root_fd, name, name)


def secure_output(path, uid, gid):
    os.chmod(path, 0o600)
    os.chown(path, uid, gid)


def main():
    if len(sys.argv) != 2:
        fail("usage: e2b_snapshot.py REQUEST.json")
    with open(sys.argv[1], "r", encoding="utf-8") as source:
        request = json.load(source)
    root = request["root"]
    uid = int(request["uid"])
    gid = int(request["gid"])
    root_fd = os.open(root, os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        operation = request["operation"]
        if operation == "scan":
            entries = []
            scan_directory(root_fd, "", entries)
            output = request["output"]
            temporary = output + ".incoming"
            with open(temporary, "w", encoding="utf-8") as destination:
                json.dump(entries, destination, ensure_ascii=False, separators=(",", ":"))
                destination.write("\n")
                destination.flush()
                os.fsync(destination.fileno())
            os.replace(temporary, output)
            secure_output(output, uid, gid)
        elif operation == "bundle":
            output = request["output"]
            tar_regular_files(root_fd, request["paths"], output)
            secure_output(output, uid, gid)
        elif operation == "captures":
            for capture in request["captures"]:
                tar_capture(root_fd, capture["guest_path"], capture["output"])
                secure_output(capture["output"], uid, gid)
        else:
            fail(f"unsupported operation {operation!r}")
    finally:
        os.close(root_fd)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"agentlab E2B snapshot helper: {error}", file=sys.stderr)
        sys.exit(1)
