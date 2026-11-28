"""Draw Synclone's placeholder icon (a white cloud on a blue rounded square)
and write the PNG and ICO files Tauri needs. Pure standard library, so it
runs anywhere: python scripts/make-icons.py
"""
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "synclone-app", "icons")


def inside_rounded_square(x, y, n):
    r = n * 0.22
    cx = min(max(x, r), n - r)
    cy = min(max(y, r), n - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def inside_cloud(x, y, n):
    s = n / 100.0
    circles = [(36, 56, 16), (52, 46, 21), (68, 57, 14)]
    for cx, cy, r in circles:
        if (x - cx * s) ** 2 + (y - cy * s) ** 2 <= (r * s) ** 2:
            return True
    return 30 * s <= x <= 70 * s and 52 * s <= y <= 71 * s


def pixel(x, y, n):
    # 4x supersampling for smooth edges.
    acc = [0, 0, 0, 0]
    for dy in (0.125, 0.375, 0.625, 0.875):
        for dx in (0.125, 0.375, 0.625, 0.875):
            px, py = x + dx, y + dy
            if inside_rounded_square(px, py, n):
                c = (255, 255, 255) if inside_cloud(px, py, n) else (37, 99, 235)
                acc[0] += c[0]
                acc[1] += c[1]
                acc[2] += c[2]
                acc[3] += 255
    return tuple(v // 16 for v in acc)


def png(n):
    rows = b""
    for y in range(n):
        rows += b"\x00" + b"".join(bytes(pixel(x, y, n)) for x in range(n))

    def chunk(kind, data):
        c = kind + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


def ico(images):
    head = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    data = b""
    for n, blob in images:
        entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return head + entries + data


def main():
    os.makedirs(OUT, exist_ok=True)
    pngs = {n: png(n) for n in (16, 32, 48, 128, 256)}
    for n, name in ((32, "32x32.png"), (128, "128x128.png"), (256, "icon.png")):
        with open(os.path.join(OUT, name), "wb") as f:
            f.write(pngs[n])
    with open(os.path.join(OUT, "icon.ico"), "wb") as f:
        f.write(ico([(n, pngs[n]) for n in (16, 32, 48, 256)]))
    print("icons written to", os.path.normpath(OUT))


if __name__ == "__main__":
    main()
