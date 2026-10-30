#!/usr/bin/env python3
"""Builds the fonts bundled with the app (crates/merpati-app/assets/fonts) and
the icon constants (crates/merpati-app/src/icons.rs).

    python3 -m venv /tmp/ft && /tmp/ft/bin/pip install fonttools
    /tmp/ft/bin/python scripts/build-fonts.py --inter DIR --jetbrains DIR --phosphor DIR

--inter      folder with Inter-Regular/Medium/SemiBold.ttf (Inter 4.x release, extras/ttf)
--jetbrains  folder with JetBrainsMono-Regular.ttf (JetBrains Mono 2.x release, fonts/ttf)
             (both optional: leave them out to rebuild only the icon font)
--phosphor   folder with Phosphor.ttf and codepoints.rs (the egui-phosphor crate's
             res/ and src/variants/, or the Phosphor release)

The text fonts are cut down to the scripts the UI needs (Latin, punctuation,
arrows, box drawing, ...); other characters fall back to egui's default fonts.
The icon font keeps only the icons listed in ICONS. All fonts are SIL OFL 1.1
(Inter, JetBrains Mono) or MIT (Phosphor); their licenses are copied along.
"""

import argparse
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "merpati-app", "assets", "fonts")
ICONS_RS = os.path.join(ROOT, "crates", "merpati-app", "src", "icons.rs")

TEXT_RANGES = ",".join([
    "U+0000-024F",  # Basic Latin, Latin-1, Latin Extended-A/B
    "U+0259", "U+02B0-02FF", "U+0300-036F",
    "U+1E00-1EFF",  # Latin Extended Additional (Vietnamese, ...)
    "U+2000-206F",  # General punctuation
    "U+20A0-20CF",  # Currency
    "U+2100-214F",  # Letterlike
    "U+2190-21FF",  # Arrows
    "U+2200-22FF",  # Math operators
    "U+2300-23FF",  # Misc technical (⌘ ⌥ ⇧ ⏎ ...)
    "U+2460-24FF", "U+2500-25FF",  # Enclosed, box drawing, blocks, geometric shapes
    "U+2600-26FF", "U+2700-27BF",  # Misc symbols, dingbats (✓ ✗ ...)
    "U+FB00-FB06", "U+FFFD",
])

# Icons the app uses (Phosphor names); add one here, rerun, then use icons::NAME.
ICONS = """
ARCHIVE ARROWS_CLOCKWISE ARROWS_DOWN_UP ARROWS_IN ARROWS_LEFT_RIGHT ARROWS_OUT ARROW_BEND_UP_LEFT
ARROW_CLOCKWISE ARROW_COUNTER_CLOCKWISE ARROW_DOWN ARROW_LEFT ARROW_LINE_DOWN ARROW_RIGHT
ARROW_SQUARE_OUT ARROW_UP ARROWS_IN_LINE_VERTICAL ARROWS_OUT_LINE_VERTICAL BELL BOOKMARK_SIMPLE BOOK_OPEN BRACKETS_ANGLE BRACKETS_CURLY BRAIN
BROADCAST BROOM BUG CARDS CARET_DOWN CARET_LEFT CARET_RIGHT CARET_UP CHART_BAR CHATS CHAT_CIRCLE CHECK
CHECK_CIRCLE CHECK_SQUARE CIRCLE CIRCLE_HALF CIRCLE_NOTCH CLIPBOARD CLIPBOARD_TEXT CLOCK
CLOCK_COUNTER_CLOCKWISE CLOUD_ARROW_DOWN CLOUD_ARROW_UP CODE COLUMNS COMMAND COOKIE COPY CPU CROSSHAIR
CUBE DATABASE DESKTOP DIAMOND DOTS_SIX_VERTICAL DOTS_THREE DOTS_THREE_VERTICAL DOWNLOAD
DOWNLOAD_SIMPLE ENVELOPE ERASER EXPORT EYE EYE_SLASH FILE FILES FILE_ARROW_DOWN FILE_ARROW_UP
FILE_CODE FILE_PLUS FILE_TEXT FINGERPRINT FLOPPY_DISK FLOW_ARROW FOLDER FOLDERS FOLDER_OPEN FOLDER_PLUS
FOLDER_SIMPLE FUNNEL GAUGE GEAR GEAR_SIX GIFT GIT_BRANCH GIT_DIFF GIT_FORK GIT_MERGE GLOBE
GLOBE_SIMPLE GRAPH HASH HEART HOURGLASS HOUSE INFO KEY KEYBOARD LIGHTBULB LIGHTNING LINK
LINK_SIMPLE LIST LIST_CHECKS LOCK LOCK_KEY LOCK_SIMPLE MAGNIFYING_GLASS MEGAPHONE MINUS MONITOR MOON
NOTE NOTEPAD PACKAGE PAPER_PLANE_RIGHT PATH PAUSE PENCIL_SIMPLE PLAY PLAY_CIRCLE PLUG PLUGS
PLUGS_CONNECTED PLUS PUSH_PIN PUZZLE_PIECE QUESTION RADIO ROBOT ROCKET ROCKET_LAUNCH ROWS SCROLL
SHARE SHIELD_CHECK SIDEBAR SIDEBAR_SIMPLE SIGN_IN SIGN_OUT SLIDERS SLIDERS_HORIZONTAL SORT_ASCENDING
SPARKLE SPINNER SPLIT_HORIZONTAL SPLIT_VERTICAL SQUARE SQUARES_FOUR STACK STAR STOP SUN TABLE TAG
TERMINAL TERMINAL_WINDOW TEST_TUBE TEXT_AA TEXT_ALIGN_LEFT TIMER TRANSLATE TRASH TREE_STRUCTURE
TREE_VIEW UPLOAD UPLOAD_SIMPLE USER VAULT WARNING WARNING_CIRCLE WEBHOOKS_LOGO WIFI_HIGH X X_CIRCLE
""".split()


def run(cmd):
    print(" ".join(cmd))
    subprocess.run(cmd, check=True)


def subset(src, dst, args):
    run([sys.executable, "-m", "fontTools.subset", src, f"--output-file={dst}",
         "--layout-features=kern,liga,calt,tnum", "--no-hinting", "--desubroutinize",
         "--name-IDs=0,1,2,3,4,5,6,13,14", "--drop-tables+=DSIG,STAT"] + args)


def copy_license(src, name):
    with open(src, "rb") as f:
        data = f.read()
    with open(os.path.join(OUT, name), "wb") as f:
        f.write(data)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--inter")
    p.add_argument("--jetbrains")
    p.add_argument("--phosphor", required=True)
    p.add_argument("--inter-license", default=None)
    p.add_argument("--jetbrains-license", default=None)
    a = p.parse_args()
    os.makedirs(OUT, exist_ok=True)

    if a.inter:
        for weight in ["Regular", "Medium", "SemiBold"]:
            subset(os.path.join(a.inter, f"Inter-{weight}.ttf"), os.path.join(OUT, f"Inter-{weight}.ttf"),
                   [f"--unicodes={TEXT_RANGES}"])
    if a.jetbrains:
        subset(os.path.join(a.jetbrains, "JetBrainsMono-Regular.ttf"),
               os.path.join(OUT, "JetBrainsMono-Regular.ttf"), [f"--unicodes={TEXT_RANGES}"])

    codepoints = {}
    with open(os.path.join(a.phosphor, "codepoints.rs"), encoding="utf-8") as f:
        for m in re.finditer(r'pub const ([A-Z0-9_]+): &str = "\\u\{([0-9A-F]+)\}";', f.read()):
            codepoints[m.group(1)] = m.group(2)
    missing = [n for n in ICONS if n not in codepoints]
    if missing:
        sys.exit(f"unknown icons: {missing}")
    used = sorted(set(ICONS))
    # a-z stay so that skrifa's autohinter classifies the icons as Latin (see egui-phosphor's subset.rs).
    unicodes = ",".join(["U+0061-007A"] + [f"U+{codepoints[n]}" for n in used])
    subset(os.path.join(a.phosphor, "Phosphor.ttf"), os.path.join(OUT, "Phosphor.ttf"), [f"--unicodes={unicodes}"])

    with open(ICONS_RS, "w", encoding="utf-8") as f:
        f.write("//! Icons of the bundled Phosphor subset (generated by `scripts/build-fonts.py`;\n")
        f.write("//! add names there). Use them inside ordinary text: `format!(\"{} New\", icons::PLUS)`.\n\n")
        f.write("#![allow(dead_code)]\n\n")
        for n in used:
            f.write(f'pub const {n}: &str = "\\u{{{codepoints[n]}}}";\n')
    print(f"{len(used)} icons, fonts in {OUT}")


if __name__ == "__main__":
    main()
