#!/usr/bin/env python3
"""Check exact ALSA playback descriptor observations from the serial log."""
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text(errors="replace").replace("\r", "")
expected = {
    "normal": ("1", [("1", "S16_LE", "0x01 (1 OUT) (SYNC)", "48000, 96000", None)]),
    "badd": ("2", [("1", "S16_LE", "0x01 (1 OUT) (ASYNC)", "48000 - 48000 (continuous)", "0x81 (1 IN)"), ("2", "S24_3LE", "0x01 (1 OUT) (ASYNC)", "48000 - 48000 (continuous)", "0x81 (1 IN)")]),
    "full-uac3-clock": ("3", [("1", "S16_LE", "0x01 (1 OUT) (SYNC)", "48000, 96000", None)]),
}
assert "ALSA_FAIL" not in text, "guest reported failure"
assert text.count("ALSA_PROFILES_COMPLETE") == 1, "missing aggregate"
assert "reboot: Power down" in text, "guest did not power down"
for profile, (config, rows) in expected.items():
    pattern = rf"^ALSA_PROFILE_BEGIN {re.escape(profile)} config={config}\n(.*?)^ALSA_PROFILE_END {re.escape(profile)}$"
    blocks = re.findall(pattern, text, re.M | re.S)
    assert len(blocks) == 1, f"{profile}: missing or duplicate profile"
    playback = blocks[0].split("Playback:\n", 1)
    assert len(playback) == 2, f"{profile}: no playback"
    sections = re.split(r"(?=^  Interface )", playback[1], flags=re.M)[1:]
    actual = []
    for section in sections:
        def field(name):
            matches = re.findall(rf"^    {re.escape(name)}: (.+)$", section, re.M)
            assert len(matches) == 1, f"{profile}: missing or duplicate {name}"
            return matches[0]
        alts = re.findall(r"^    Altset (\d+)$", section, re.M)
        assert len(alts) == 1, f"{profile}: missing or duplicate alternate"
        alt = alts[0]
        assert field("Channels") == "2", f"{profile}: channel mismatch"
        sync = re.findall(r"^    Sync Endpoint: (.+)$", section, re.M)
        assert len(sync) <= 1, f"{profile}: duplicate sync endpoint"
        actual.append((alt, field("Format"), field("Endpoint"), field("Rates"), sync[0] if sync else None))
    assert actual == rows, f"{profile}: expected {rows!r}, got {actual!r}"
    print(f"ALSA_PROFILE_PASS {profile} {actual!r}")
