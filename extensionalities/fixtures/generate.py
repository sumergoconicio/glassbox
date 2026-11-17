"""Reproducible, self-authored tiny fixtures. No downloaded documents."""
from pathlib import Path
import struct
import zlib
import zipfile

ROOT = Path(__file__).resolve().parent


def archive(name, entries):
    with zipfile.ZipFile(ROOT / name, "w", zipfile.ZIP_DEFLATED) as out:
        for path, body in entries.items():
            info = zipfile.ZipInfo(path, (2026, 1, 2, 3, 4, 6))
            info.compress_type = zipfile.ZIP_DEFLATED
            out.writestr(info, body)


def png(w, h):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress((b"\0" + b"\xff\xff\xff" * w) * h))
            + chunk(b"IEND", b""))


archive("listing.zip", {"note.txt": "Hello archive\n", "../escape.txt": "Never extract me\n"})
(ROOT / "image.png").write_bytes(png(640, 400))
(ROOT / "disguised.pdf").write_bytes(png(10, 10))
(ROOT / "truncated.zip").write_bytes(b"PK\x03\x04broken")
archive("entry-cap.zip", {f"entry-{i}": "" for i in range(4097)})
archive("bomb.zip", {"huge.bin": b""})
bomb = bytearray((ROOT / "bomb.zip").read_bytes())
central = bomb.index(b"PK\x01\x02")
struct.pack_into("<I", bomb, central + 24, 257 << 20)
(ROOT / "bomb.zip").write_bytes(bomb)
