#!/usr/bin/env python3
"""SC8: a microphone PCM capture from the board, as a WAV file and its
dominant frequency.

Reads `[samples] KEY mic i16 ...` lines, one per sample batch, as
`mwctl monitor --samples` prints them (USB CDC or LPUART1) and as
`cargo run --example zenoh_board -- --samples-lines` prints them (Zenoh),
from a file or stdin:

    mwctl --port /dev/ttyACM1 monitor --samples --seconds 10 \\
        | python3 -I tools/mic_capture.py --expect 1000 --wav tone.wav

Batches must be one channel of i16 at one rate; their `first` indices say
where each goes, so a missing batch leaves zeros and is reported as a gap.
Prints the capture's length, gaps, RMS, the dominant frequency (Hann
window, parabolic interpolation of the peak bin, ignoring < 50 Hz) and the
tone's SNR (power within --tol of the peak against everything else from
50 Hz to Nyquist). With --expect, exits 1 unless the dominant frequency is
within --tol Hz of it.
"""
import argparse
import re
import sys
import wave

import numpy as np

HEADER = re.compile(r"(\w+)=(-?\d+)")
MIN_HZ = 50.0


def parse(lines, key):
    """The batches of `key`: (rate, [(first, values)])."""
    rate = None
    batches = []
    for line in lines:
        if not line.startswith("[samples] "):
            continue
        head, _, body = line.rstrip("\n").partition(": ")
        words = head.split()
        if len(words) < 4 or words[1] != key:
            continue
        if words[2] != "mic" or words[3] != "i16":
            sys.exit(f"{key}: not microphone PCM ({words[2]} {words[3]})")
        h = {k: int(v) for k, v in HEADER.findall(head)}
        if h.get("ch") != 1:
            sys.exit(f"{key}: {h.get('ch')} channels, expected 1")
        if rate is None:
            rate = h["rate"]
        elif h["rate"] != rate:
            sys.exit(f"{key}: rate changed from {rate} to {h['rate']}")
        values = [int(v) for v in body.split()] if body else []
        if len(values) != h["n"]:
            sys.exit(f"{key}: batch at {h['first']} has {len(values)} values, header says {h['n']}")
        batches.append((h["first"], values))
    return rate, batches


def assemble(batches):
    """One signal from the batches placed by index; (signal, gap units)."""
    batches.sort()
    start = batches[0][0]
    end = max(f + len(v) for f, v in batches)
    signal = np.zeros(end - start, dtype=np.int16)
    covered = 0
    for first, values in batches:
        signal[first - start:first - start + len(values)] = values
        covered += len(values)
    return signal, (end - start) - covered


def analyse(signal, rate, tol):
    """(dominant Hz, SNR dB, spectrum peak bin) of `signal`."""
    x = signal.astype(np.float64)
    x -= x.mean()
    w = np.hanning(len(x))
    spec = np.abs(np.fft.rfft(x * w)) ** 2
    freqs = np.fft.rfftfreq(len(x), 1.0 / rate)
    band = freqs >= MIN_HZ
    k = int(np.argmax(np.where(band, spec, 0.0)))
    # Parabolic interpolation on the log magnitude around the peak bin.
    if 0 < k < len(spec) - 1:
        a, b, c = np.log(spec[k - 1:k + 2] + 1e-30)
        shift = 0.5 * (a - c) / (a - 2 * b + c) if (a - 2 * b + c) != 0 else 0.0
    else:
        shift = 0.0
    peak_hz = (k + shift) * rate / len(x)
    tone = band & (np.abs(freqs - peak_hz) <= tol)
    rest = band & ~tone
    snr = 10 * np.log10(spec[tone].sum() / max(spec[rest].sum(), 1e-30))
    return peak_hz, snr


def main():
    ap = argparse.ArgumentParser(description="SC8: a microphone PCM capture as WAV and its dominant frequency")
    ap.add_argument("--input", default="-", help="file of [samples] lines, - for stdin")
    ap.add_argument("--key", default="mic/pcm")
    ap.add_argument("--wav", help="write the capture as a 16-bit mono WAV")
    ap.add_argument("--expect", type=float, help="the tone's frequency in Hz")
    ap.add_argument("--tol", type=float, default=20.0, help="Hz [20]")
    a = ap.parse_args()
    src = sys.stdin if a.input == "-" else open(a.input, encoding="utf-8", errors="replace")
    rate, batches = parse(src, a.key)
    if rate is None or not batches:
        sys.exit(f"no {a.key} batches in the input")
    signal, gap = assemble(batches)
    seconds = len(signal) / rate
    rms = float(np.sqrt(np.mean(signal.astype(np.float64) ** 2)))
    print(f"{a.key}: {len(batches)} batches, {len(signal)} samples at {rate} Hz "
          f"({seconds:.2f} s), {gap} missing, RMS {rms:.0f}, peak {int(np.abs(signal.astype(np.int32)).max())}")
    if a.wav:
        with wave.open(a.wav, "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(rate)
            wf.writeframes(signal.astype("<i2").tobytes())
        print(f"wrote {a.wav}")
    peak_hz, snr = analyse(signal, rate, a.tol)
    print(f"dominant {peak_hz:.1f} Hz, SNR {snr:.1f} dB (within {a.tol:g} Hz vs the rest above {MIN_HZ:g} Hz)")
    if a.expect is not None:
        ok = abs(peak_hz - a.expect) <= a.tol
        print(("PASS" if ok else "FAIL") + f": expected {a.expect:g} ± {a.tol:g} Hz")
        sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
