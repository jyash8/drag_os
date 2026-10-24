#!/usr/bin/env python3
import os
import sys
import subprocess
import numpy as np

def build_vid():
    video_in = "video.mp4"
    vid_out = "initrd/video.vid"
    
    if not os.path.exists(video_in):
        print(f"Error: {video_in} not found.")
        sys.exit(1)

    fps = 24
    width = 320
    height = 200
    sample_rate = 44100
    samples_per_frame = sample_rate // fps # 1837 samples (3674 bytes) per frame

    print("Extracting audio stream (44.1kHz 16-bit mono PCM)...")
    cmd_audio = [
        "ffmpeg", "-y", "-i", video_in,
        "-vn", "-acodec", "pcm_s16le", "-ar", str(sample_rate), "-ac", "1",
        "-f", "s16le", "-"
    ]
    proc_audio = subprocess.Popen(cmd_audio, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    audio_data, _ = proc_audio.communicate()

    print(f"Extracting video frames ({width}x{height} @ {fps} fps)...")
    cmd_video = [
        "ffmpeg", "-y", "-i", video_in,
        "-vf", f"scale={width}:{height},format=gray",
        "-r", str(fps),
        "-f", "rawvideo", "-"
    ]
    proc_video = subprocess.Popen(cmd_video, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    video_data, _ = proc_video.communicate()

    frame_bytes = width * height
    num_video_frames = len(video_data) // frame_bytes
    total_audio_samples = len(audio_data) // 2
    num_audio_frames = total_audio_samples // samples_per_frame

    num_frames = min(num_video_frames, num_audio_frames)
    print(f"Total synced frames: {num_frames} ({num_frames / fps:.2f} seconds)")

    print(f"Packing video.vid stream to {vid_out}...")
    with open(vid_out, "wb") as f_out:
        # Header: 'DRGV', width(u16), height(u16), fps(u16), total_frames(u32)
        header = b'DRGV' + width.to_bytes(2, 'little') + height.to_bytes(2, 'little') + fps.to_bytes(2, 'little') + num_frames.to_bytes(4, 'little')
        f_out.write(header)

        for f_idx in range(num_frames):
            # 1. Write audio chunk (2940 samples = 5880 bytes)
            a_start = f_idx * samples_per_frame * 2
            a_end = a_start + samples_per_frame * 2
            audio_chunk = audio_data[a_start:a_end]
            f_out.write(audio_chunk)

            # 2. Pack video frame into 1-bit monochrome (8,000 bytes)
            v_start = f_idx * frame_bytes
            v_end = v_start + frame_bytes
            raw_frame = np.frombuffer(video_data[v_start:v_end], dtype=np.uint8)
            # Threshold at 128
            mono_bits = (raw_frame > 128).astype(np.uint8)
            # Pack 8 pixels per byte
            packed_frame = np.packbits(mono_bits)
            f_out.write(packed_frame.tobytes())

    print(f"Successfully generated {vid_out} (size: {os.path.getsize(vid_out)} bytes)")

if __name__ == "__main__":
    build_vid()
