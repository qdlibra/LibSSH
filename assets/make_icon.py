#!/usr/bin/env python3
"""Generate platform icon assets from assets/icon.png."""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
SOURCE = ASSETS / "icon.png"
PNG_512 = ASSETS / "icon@512.png"
ICO = ASSETS / "LibSSH.ico"
ICNS = ASSETS / "LibSSH.icns"


def generate_png_and_ico() -> None:
    try:
        from PIL import Image
    except ImportError as exc:
        raise SystemExit("Pillow is required: python3 -m pip install pillow") from exc

    image = Image.open(SOURCE).convert("RGBA")
    image.resize((512, 512), Image.Resampling.LANCZOS).save(PNG_512)
    image.save(ICO, sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])


def generate_icns_if_available() -> None:
    if shutil.which("sips") is None or shutil.which("iconutil") is None:
        return

    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "LibSSH.iconset"
        iconset.mkdir()
        sizes = [
            (16, "icon_16x16.png"),
            (32, "icon_16x16@2x.png"),
            (32, "icon_32x32.png"),
            (64, "icon_32x32@2x.png"),
            (128, "icon_128x128.png"),
            (256, "icon_128x128@2x.png"),
            (256, "icon_256x256.png"),
            (512, "icon_256x256@2x.png"),
            (512, "icon_512x512.png"),
        ]
        for size, name in sizes:
            subprocess.run(
                ["sips", "-z", str(size), str(size), str(PNG_512), "--out", str(iconset / name)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        shutil.copy2(PNG_512, iconset / "icon_512x512@2x.png")
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(ICNS)], check=True)


def main() -> None:
    if not SOURCE.exists():
        raise SystemExit(f"Missing source icon: {SOURCE}")
    generate_png_and_ico()
    generate_icns_if_available()
    print(f"wrote {PNG_512.relative_to(ROOT)}")
    print(f"wrote {ICO.relative_to(ROOT)}")
    if ICNS.exists():
        print(f"wrote {ICNS.relative_to(ROOT)}")


if __name__ == "__main__":
    sys.exit(main())
