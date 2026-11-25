"""Generate the colour-temperature-to-XY wire lookup from the pinned converter checkout."""
import argparse
from pathlib import Path
import re


def generate(source):
    rows = re.findall(r"^\s*(\d+): \{x: ([\d.]+), y: ([\d.]+)\},$", source, re.M)
    if [int(k) for k, _, _ in rows] != list(range(1000, 20001, 10)):
        raise ValueError("unexpected Kelvin table coverage")
    return b"".join(int(float(v) * 65535 + 0.5).to_bytes(2, "little")
                    for _, x, y in rows for v in (x, y))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.write_bytes(generate((args.checkout / "src/lib/kelvinToXy.ts").read_text()))
