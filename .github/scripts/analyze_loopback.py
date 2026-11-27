#!/usr/bin/env python3
"""Report measured waveform discontinuity separately from contiguous silence.

Input is a lossless PCM WAV plus its capture metadata and JSON event timeline:
[{"seconds": 1.25, "event": "start|stop|alt-zero|rate-switch"}]. No metric by
itself establishes a device pass, audibility, or an absolute voltage level.
"""
import argparse
import hashlib
import json
import math
import pathlib
import wave


def dbfs(value):
    return 20 * math.log10(value) if value > 0 else None


def analyze(path, events, window_ms, silence_dbfs, alignment_offset_ms):
    with wave.open(str(path), "rb") as wav:
        rate, channels, width = wav.getframerate(), wav.getnchannels(), wav.getsampwidth()
        if wav.getcomptype() != "NONE" or width not in (2, 3, 4):
            raise ValueError("expected signed integer PCM16/24/32 WAV")
        data = wav.readframes(wav.getnframes())
    scale = float(1 << (width * 8 - 1))
    samples = [int.from_bytes(data[i:i + width], "little", signed=True) / scale for i in range(0, len(data), width)]
    frames = [samples[i:i + channels] for i in range(0, len(samples), channels)]
    threshold = 10 ** (silence_dbfs / 20)
    results = []
    for event in events:
        if event["event"] not in {"start", "stop", "alt-zero", "rate-switch"}:
            raise ValueError("unrecognized playback event")
        center = round((float(event["seconds"]) + alignment_offset_ms / 1000) * rate)
        radius = max(1, round(window_ms * rate / 1000))
        if center < radius or center + radius >= len(frames):
            raise ValueError("event window falls outside recorded audio")
        selected = frames[center - radius:center + radius + 1]
        flat = [sample for frame in selected for sample in frame]
        jump = max(abs(current[c] - previous[c]) for previous, current in zip(selected, selected[1:]) for c in range(channels))
        quiet = [max(map(abs, frame)) <= threshold for frame in selected]
        nearest = radius if quiet[radius] else None
        if nearest is None:
            silence_ms = 0.0
        else:
            left = right = nearest
            while left > 0 and quiet[left - 1]: left -= 1
            while right + 1 < len(quiet) and quiet[right + 1]: right += 1
            silence_ms = (right - left + 1) * 1000 / rate
        results.append({"event":event["event"],"capture_center_frame":center,"window_ms":window_ms,"peak_dbfs":dbfs(max(map(abs, flat))),"rms_dbfs":dbfs(math.sqrt(sum(x*x for x in flat)/len(flat))),"max_adjacent_sample_jump_dbfs":dbfs(jump),"clipped_samples":sum(abs(x)>=1-1/scale for x in flat),"silence_at_event_ms":silence_ms,"silence_threshold_dbfs":silence_dbfs,"silence_window_truncated":bool(nearest is not None and (left == 0 or right == len(quiet)-1))})
    return {"sample_rate":rate,"channels":channels,"sample_bits":width*8,"results":results}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wav", type=pathlib.Path)
    parser.add_argument("events", type=pathlib.Path)
    parser.add_argument("--window-ms", type=float, default=100)
    parser.add_argument("--silence-dbfs", type=float, default=-80)
    parser.add_argument("--alignment-offset-ms", type=float, required=True)
    parser.add_argument("--reference-rate", type=int, required=True)
    parser.add_argument("--clock-aligned", action="store_true")
    parser.add_argument("--capture-gain-db", type=float, required=True)
    args = parser.parse_args()
    if not all(math.isfinite(x) for x in [args.window_ms,args.silence_dbfs,args.alignment_offset_ms,args.capture_gain_db]) or args.window_ms <= 0:
        parser.error("metrics require finite settings and a positive window")
    metadata = json.loads(pathlib.Path(str(args.wav) + ".json").read_text())
    measured = analyze(args.wav, json.loads(args.events.read_text()), args.window_ms, args.silence_dbfs, args.alignment_offset_ms)
    if measured["sample_rate"] != args.reference_rate or metadata["sample_rate"] != args.reference_rate:
        parser.error("capture, WAV and reference sample rates must match")
    if not metadata.get("capture_complete"):
        parser.error("capture has losses or lacks complete-recording evidence")
    measured.update({"wav_sha256":hashlib.sha256(args.wav.read_bytes()).hexdigest(),"clock_aligned":args.clock_aligned,"alignment_offset_ms":args.alignment_offset_ms,"capture_gain_db":args.capture_gain_db,"comparable":args.clock_aligned,"limits":"ADC-relative dBFS; compare only with identical calibrated capture chain, gain and clock alignment. Silence duration and waveform jumps are separate metrics, not audibility or pass judgments."})
    print(json.dumps(measured, indent=2, allow_nan=False))


if __name__ == "__main__":
    main()
