#!/usr/bin/env python3
"""Lists UI string literals in crates/merpati-app that bypass the translation
system (`tr`/`trf`), and `tr` keys missing from i18n/id.tsv.

    python3 scripts/i18n-scan.py            # report
    python3 scripts/i18n-scan.py --keys     # only print missing tr() keys

Calls may span several lines, and a `format!` passed straight to a UI call
counts as a literal (use `trf` with `{}` placeholders instead).
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent / "crates" / "merpati-app"
SRC = ROOT / "src"
TSV = ROOT / "i18n" / "id.tsv"

# Calls whose first string argument is shown to the user.
UI_CALLS = [
    r"\.label\(", r"\.heading\(", r"\.button\(", r"\.small_button\(", r"\.hint_text\(",
    r"\.on_hover_text\(", r"\.on_disabled_hover_text\(", r"\.selectable_label\([^,()]+,\s*",
    r"\.checkbox\([^,()]+,\s*", r"\.radio_value\([^,()]+,[^,()]+,\s*", r"\.selectable_value\([^,()]+,[^,()]+,\s*",
    r"\.toggle_value\([^,()]+,\s*", r"\.selected_text\(", r"RichText::new\(", r"muted\(\s*ui,\s*",
    r"error_text\(\s*ui,\s*", r"accent_button\(\s*ui,\s*&?", r"\btoast\(", r"\.collapsing\(",
    r"CollapsingHeader::new\(", r"menu_button\(", r"\.text\(", r"Button::new\(", r"Label::new\(",
    r"\.link\(", r"\.hyperlink_to\(",
    # Lines of the protocol tabs' message logs.
    r"\.log\.system\(", r"\.log\.error\(",
]
LIT = r'"((?:[^"\\]|\\.)*)"'
# `format!("…", …)` passed directly is as untranslated as a literal.
CALL_RE = re.compile("(?:" + "|".join(UI_CALLS) + r")\s*&?(?:format!\(\s*)?" + LIT)
# Text that is not prose: symbols, shortcuts, protocol names, code.
NOT_PROSE = re.compile(
    r"^[\W\d_]*$|^[A-Z0-9_\-/.:{}<> ]{1,6}$|^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS|JSON|XML|HTML|YAML|gRPC|MQTT|MCP|URL|ID|OK|AI)$"
)
# Placeholders (`{}`, `{name:.1}`), escaped braces and escapes such as `\n`.
NOT_TEXT = re.compile(r"\{\{|\}\}|\{[^{}]*\}|\\.")


def table():
    keys = set()
    for line in TSV.read_text().splitlines():
        if line and not line.startswith("#") and "\t" in line:
            keys.add(line.split("\t", 1)[0])
    return keys


def unescape(s):
    """A Rust string literal's text as it appears in the table."""
    return s.replace('\\"', '"').replace("\\\\", "\\")


def code_of(text):
    """The non-test code of a file with comment lines blanked (line numbers kept)."""
    out = []
    in_tests = False
    lines = text.splitlines()
    for i, line in enumerate(lines):
        # The test module (a `#[cfg(test)] impl` may come before other code).
        if line.startswith("#[cfg(test)]") and i + 1 < len(lines) and lines[i + 1].startswith("mod "):
            in_tests = True
        if in_tests or line.strip().startswith("//"):
            out.append("")
        else:
            out.append(line)
    return "\n".join(out)


def main():
    keys = table()
    untranslated, missing = [], set()
    for path in sorted(SRC.glob("*.rs")):
        code = code_of(path.read_text())
        for m in re.finditer(r'(?<![\w])trf?\(\s*"((?:[^"\\]|\\.)*)"', code):
            key = m.group(1)
            if unescape(key) not in keys:
                missing.add(key)
        for m in CALL_RE.finditer(code):
            s = m.group(1)
            if NOT_PROSE.match(s) or not re.search(r"[A-Za-z]{2}", NOT_TEXT.sub(" ", s)):
                continue
            n = code.count("\n", 0, m.start(1)) + 1
            untranslated.append(f"{path.name}:{n}: {s}")
    if "--keys" in sys.argv:
        print("\n".join(sorted(missing)))
        return
    print(f"{len(untranslated)} literals outside tr(), {len(missing)} tr() keys without a translation")
    for u in untranslated:
        print("  " + u)
    for k in sorted(missing):
        print("  missing: " + k)


main()
