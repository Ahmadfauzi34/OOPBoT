#!/usr/bin/env python3
"""Verify Compose compiler stability and skippability metrics emitted during build."""

from __future__ import annotations

import pathlib
import re
import sys

REQUIRED_STABLE_CLASSES = (
    "TimelineState",
    "TimelineItem.User",
    "TimelineItem.AssistantText",
    "TimelineItem.Thinking",
    "TimelineItem.ToolCall",
    "TimelineItem.Error",
    "TimelineItem.ExtensionMessage",
    "TimelineItem.ExtensionNotice",
    "TimelineItem.Notice",
    "TimelineItem.TurnFooter",
    "TimelineItem.ExitBanner",
    "TimelineItem.Steps",
    "DiffDocument",
    "SideHighlight",
    "MarkdownCache",
    "DiffSection",
    "TurnStatus",
    "ChatScroll",
)

REQUIRED_SKIPPABLE_COMPOSABLES = (
    "StreamItem",
    "AssistantMessage",
    "UserMessage",
    "StepFoldRow",
    "WorkspacePane",
    "WorkspacePaneSlot",
    "OopbotMarkdown",
    "OopbotMarkdownContent",
)


def parse_classes(text: str) -> dict[str, str]:
    classes: dict[str, str] = {}
    pattern = re.compile(r"^(stable|unstable|runtime)\s+class\s+([^\s{]+)")
    for line in text.splitlines():
        match = pattern.match(line.strip())
        if match:
            stability, name = match.groups()
            classes[name] = stability
    return classes


def parse_composables(text: str) -> dict[str, list[str]]:
    composables: dict[str, list[str]] = {}
    pattern = re.compile(r"^(.*?)\bfun\s+([^\s(]+)\(")
    for line in text.splitlines():
        stripped = line.strip()
        match = pattern.match(stripped)
        if match:
            modifiers, name = match.groups()
            composables.setdefault(name, []).append(modifiers)
    return composables


def main() -> int:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <compose-compiler-reports-dir>", file=sys.stderr)
        return 2

    reports_dir = pathlib.Path(sys.argv[1])
    class_files = sorted(reports_dir.glob("*-classes.txt"))
    composable_files = sorted(reports_dir.glob("*-composables.txt"))
    if not class_files or not composable_files:
        print(f"Missing Compose compiler reports under {reports_dir}", file=sys.stderr)
        return 1

    classes: dict[str, str] = {}
    for path in class_files:
        classes.update(parse_classes(path.read_text(encoding="utf-8")))

    composables: dict[str, list[str]] = {}
    for path in composable_files:
        for name, mods in parse_composables(path.read_text(encoding="utf-8")).items():
            composables.setdefault(name, []).extend(mods)

    errors: list[str] = []
    for name in REQUIRED_STABLE_CLASSES:
        status = classes.get(name)
        if status is None:
            # Some Kotlin versions qualify nested classes or omit package names differently; fallback suffix match.
            suffix_matches = [v for k, v in classes.items() if k.endswith(f".{name}")]
            status = suffix_matches[0] if suffix_matches else None
        if status is None:
            errors.append(f"Class '{name}' missing from Compose class stability report")
        elif status == "unstable":
            errors.append(f"Class '{name}' regressed to 'unstable' in Compose class report")

    for name in REQUIRED_SKIPPABLE_COMPOSABLES:
        sigs = composables.get(name)
        if not sigs:
            errors.append(f"Composable '{name}' missing from Compose composables report")
        elif not all("skippable" in sig for sig in sigs):
            errors.append(f"Composable '{name}' is not skippable: {sigs}")

    unstable = sorted(k for k, v in classes.items() if v == "unstable")
    print(f"Compose report summary: {len(classes)} classes ({len(unstable)} unstable), {len(composables)} composables")
    if unstable:
        print("Unstable classes reported:")
        for name in unstable[:30]:
            print(f"  - {name}")

    if errors:
        for err in errors:
            print(f"ERROR: {err}", file=sys.stderr)
        return 1

    print("All critical Compose stability and skippability assertions passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
