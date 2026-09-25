#!/usr/bin/env python3
"""Compare cua-driver's accessibility projection with Codex Computer Use's for one window.

Ours comes from `cua-driver-local call get_window_state` (live or a saved JSON file).
Theirs comes from a Codex session rollout (~/.codex/sessions/**/rollout-*.jsonl); only
full-tree outputs are used, never diffs.

Examples:
  ax_parity.py --list-rollout ROLLOUT.jsonl
  ax_parity.py --pid 997 --window-id 56917 --rollout ROLLOUT.jsonl --window-title "Max, Chidi"
  ax_parity.py --ours fixtures/messages.json --rollout ROLLOUT.jsonl --window-title "Max, Chidi"
"""
import argparse
import json
import re
import subprocess
import sys
from collections import Counter

CUA = "/Users/edkiboma/.local/bin/cua-driver-local"
TIME_RE = re.compile(r"\b\d{1,2}:\d{2}\s?[AP]M\b|\byesterday\b|\btoday\b", re.I)


def norm(text):
    text = TIME_RE.sub("", text or "").lower()
    return re.sub(r"[^a-z0-9]+", " ", text).strip()


# ---------------------------------------------------------------- ours
def load_ours(args):
    if args.ours:
        data = json.load(open(args.ours))
    else:
        # Always the full outline: a same-session earlier look would otherwise turn this into a diff.
        req = {"pid": args.pid, "window_id": args.window_id, "include_screenshot": False, "diff": False}
        out = subprocess.run([CUA, "call", "get_window_state", json.dumps(req)], capture_output=True, text=True, check=True).stdout
        data = json.loads(out)
        if args.save_ours:
            open(args.save_ours, "w").write(out)
    sc = data.get("structuredContent") or data
    if "diff" in sc:
        sys.exit("saved response is a change-only diff, not a full outline; re-capture with diff:false")
    nodes = []
    for el in sc.get("elements", []):
        # Label already carries the value for most Cocoa rows; prefer one to avoid double counting.
        text = el.get("label") or el.get("value") or ""
        nodes.append({"role": el.get("role", ""), "text": text, "actions": el.get("actions", [])})
    markdown = sc.get("tree_markdown", "")
    action_chars = sum(len(m) for m in re.findall(r"actions=\[[^\]]*\]", markdown))
    return {"nodes": nodes, "bytes": len(markdown), "action_chars": action_chars, "title": sc.get("window_title", "")}


# -------------------------------------------------------------- theirs
LINE_RE = re.compile(r"^(\t*)(\d+) (.*)$")
FIELD_RE = re.compile(r"(?:^| )(Description|Value|ID|Secondary Actions|URL|Quoted text|Placeholder): ")


def full_trees(rollout):
    """Yield (title, text) for every full accessibility tree in a rollout."""
    for line in open(rollout):
        try:
            payload = json.loads(line).get("payload") or {}
        except ValueError:
            continue
        if payload.get("type") not in ("function_call_output", "custom_tool_call_output"):
            continue
        out = payload.get("output")
        if isinstance(out, list):
            out = "\n".join(c.get("text", "") for c in out if isinstance(c, dict))
        out = str(out)
        m = re.search(r'^Window: "(.*?)", App: (.*?)\.\n', out, re.M)
        if m:
            yield m.group(1), out[m.start():]


def parse_theirs(text):
    nodes = []
    action_chars = 0
    for raw in text.splitlines()[1:]:
        m = LINE_RE.match(raw)
        if not m:
            continue
        rest = m.group(3)
        fields = list(FIELD_RE.finditer(rest))
        head = rest[: fields[0].start()] if fields else rest
        role = re.sub(r"\s*\(.*?\)\s*$", "", head).strip()
        if m.group(2) == "0" and role.startswith("standard window"):
            role = "standard window"  # root line carries the title without a field prefix
        vals = {}
        for i, f in enumerate(fields):
            end = fields[i + 1].start() if i + 1 < len(fields) else len(rest)
            vals[f.group(1)] = rest[f.end():end].strip().rstrip(",")
        action_chars += len(vals.get("Secondary Actions", ""))
        text_val = " ".join(v for k, v in vals.items() if k in ("Description", "Value", "Quoted text"))
        nodes.append({"role": role, "text": text_val, "actions": vals.get("Secondary Actions", "").split(", ") if vals.get("Secondary Actions") else []})
    return {"nodes": nodes, "bytes": len(text), "action_chars": action_chars}


# -------------------------------------------------------------- compare
def summarize(side):
    texts = {norm(n["text"]) for n in side["nodes"] if norm(n["text"])}
    text_chars = sum(len(t) for t in texts)
    return {
        "nodes": len(side["nodes"]),
        "bytes": side["bytes"],
        "text_chars": text_chars,
        "signal_ratio": text_chars / side["bytes"] if side["bytes"] else 0.0,
        "action_share": side["action_chars"] / side["bytes"] if side["bytes"] else 0.0,
        "roles": Counter(n["role"] for n in side["nodes"]),
        "texts": texts,
    }


def report(ours, theirs):
    a, b = summarize(ours), summarize(theirs)
    print(f"{'metric':<16}{'cua-driver':>14}{'codex':>14}")
    for key in ("nodes", "bytes", "text_chars"):
        print(f"{key:<16}{a[key]:>14,}{b[key]:>14,}")
    print(f"{'signal_ratio':<16}{a['signal_ratio']:>14.2%}{b['signal_ratio']:>14.2%}")
    print(f"{'action_share':<16}{a['action_share']:>14.2%}{b['action_share']:>14.2%}")
    both = a["texts"] & b["texts"]
    print(f"\ntext coverage: both={len(both)}  ours_only={len(a['texts'] - b['texts'])}  theirs_only={len(b['texts'] - a['texts'])}")
    for label, items in (("ours_only", a["texts"] - b["texts"]), ("theirs_only", b["texts"] - a["texts"])):
        print(f"\n{label} (up to 12):")
        for t in sorted(items, key=len, reverse=True)[:12]:
            print(f"  - {t[:110]}")
    print("\ntop roles:")
    print("  ours  :", ", ".join(f"{r}={c}" for r, c in a["roles"].most_common(6)))
    print("  theirs:", ", ".join(f"{r}={c}" for r, c in b["roles"].most_common(6)))


def self_test():
    theirs = parse_theirs(
        'Window: "W", App: Messages.\n'
        "0 standard window W, ID: SceneWindow, Secondary Actions: Raise\n"
        "\t1 container Description: Messages, ID: TranscriptCollectionView, Secondary Actions: Cancel, Scroll Up\n"
        "\t\t2 text entry area (settable) Value: hi there, ID: CKBalloonTextView, Secondary Actions: Cancel, Heart, Copy\n"
    )
    assert [n["role"] for n in theirs["nodes"]] == ["standard window", "container", "text entry area"], theirs["nodes"]
    assert theirs["nodes"][2]["text"] == "hi there" and theirs["nodes"][2]["actions"] == ["Cancel", "Heart", "Copy"]
    ours = {"nodes": [{"role": "AXGroup", "text": "Hi there, 1:05 PM", "actions": []}], "bytes": 100, "action_chars": 60, "title": "W"}
    a, b = summarize(ours), summarize(theirs)
    assert a["texts"] == {"hi there"} and "hi there" in b["texts"] and a["action_share"] == 0.6
    print("self-test ok")


def main():
    if "--self-test" in sys.argv:
        return self_test()
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--ours", help="saved get_window_state JSON")
    p.add_argument("--pid", type=int)
    p.add_argument("--window-id", type=int)
    p.add_argument("--save-ours", help="write the live get_window_state JSON here (fixture)")
    p.add_argument("--rollout", help="Codex rollout .jsonl")
    p.add_argument("--window-title", default="", help="substring of the Codex window title to pick")
    p.add_argument("--theirs", help="saved Codex full-tree text instead of --rollout")
    p.add_argument("--list-rollout", metavar="ROLLOUT", help="list full trees in a rollout and exit")
    args = p.parse_args()

    if args.list_rollout:
        for i, (title, text) in enumerate(full_trees(args.list_rollout)):
            print(f"[{i}] {title!r}  {len(text):,} bytes")
        return
    if not (args.ours or (args.pid and args.window_id)):
        sys.exit("need --ours FILE or --pid/--window-id")
    if args.theirs:
        theirs_text = open(args.theirs).read()
    elif args.rollout:
        picks = [t for title, t in full_trees(args.rollout) if args.window_title in title]
        if not picks:
            sys.exit("no full tree matching that window title; try --list-rollout")
        theirs_text = picks[-1]
    else:
        sys.exit("need --rollout or --theirs")
    ours = load_ours(args)
    print(f"ours window: {ours['title']!r}   theirs: {theirs_text.splitlines()[0]}\n")
    report(ours, parse_theirs(theirs_text))


if __name__ == "__main__":
    main()
