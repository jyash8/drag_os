#!/usr/bin/env python3
"""Mux color MJPEG + PCM into DRGJ container (same layout as DRGV, JPEG frames).

Usage: mkdrgj.py <input.mp4> <out.vid> [fps=24] [w=320] [h=200] [q=4]
Requires: ffmpeg on PATH.

DRGJ layout:
  header: magic "DRGJ", u16 w, u16 h, u16 fps, u32 total_frames (14 bytes)
  per frame: u32 jpeg_len, <jpeg bytes>, <pcm s16le mono 44100Hz chunk>
  pcm chunk = (44100 // fps) * 2 bytes (same math as wl_player)
"""
import os
import struct
import subprocess
import sys
import tempfile

def main():
    src, out = sys.argv[1], sys.argv[2]
    fps = int(sys.argv[3]) if len(sys.argv) > 3 else 24
    w = int(sys.argv[4]) if len(sys.argv) > 4 else 320
    h = int(sys.argv[5]) if len(sys.argv) > 5 else 200
    q = sys.argv[6] if len(sys.argv) > 6 else "4"

    tmp = tempfile.mkdtemp(prefix="drgj")
    # Video: mjpeg frames
    subprocess.run([
        "ffmpeg", "-y", "-v", "error", "-i", src,
        "-vf", f"scale={w}:{h},fps={fps}", "-c:v", "mjpeg", "-q:v", q,
        os.path.join(tmp, "f%06d.jpg"),
    ], check=True)
    # Audio: mono 44.1k s16le
    pcm_path = os.path.join(tmp, "a.pcm")
    subprocess.run([
        "ffmpeg", "-y", "-v", "error", "-i", src,
        "-ar", "44100", "-ac", "1", "-f", "s16le", pcm_path,
    ], check=True)

    frames = sorted(f for f in os.listdir(tmp) if f.endswith(".jpg"))
    n = len(frames)
    spf = 44100 // fps
    chunk = spf * 2
    pcm = open(pcm_path, "rb").read()

    total = 0
    with open(out, "wb") as f:
        f.write(b"DRGJ" + struct.pack("<HHHI", w, h, fps, n))
        for i, name in enumerate(frames):
            data = open(os.path.join(tmp, name), "rb").read()
            f.write(struct.pack("<I", len(data)))
            f.write(data)
            off = i * chunk
            piece = pcm[off:off + chunk]
            if len(piece) < chunk:
                piece += b"\x00" * (chunk - len(piece))
            f.write(piece)
            total += 4 + len(data) + chunk

    print(f"DRGJ: {n} frames {w}x{h}@{fps} + pcm -> {out} "
          f"({14 + total} bytes, audio {n * chunk} bytes)")

main()
