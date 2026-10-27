#!/usr/bin/env python3
"""Check whether a file shares code — not just an algorithm — with another tree.

  tools/provenance_scan.py <ours.rs> <theirs_dir_or_file> [--top N]

Why this exists: "we only referenced the design" is a claim, and the difference
between referencing an algorithm and copying its code is not visible by reading.
Three cheap measurements separate them:

  max contiguous token run   the longest identical token sequence. Copying leaves
                             long runs (dozens to thousands); two independent ports
                             of the same algorithm share loop headers and signatures
                             (a dozen or two tokens).
  shingle Jaccard (6-token)  overall shape overlap of the token stream.
  identical comment lines    the strongest single signal: prose is not implied by
                             an algorithm, so a shared comment is the strongest pointer for a manual look.

Calibrate before judging: run the same three numbers on a pair known to be
independent (this repo's carver against Pumpkin's carver, both written from the
same 1.21.1 bytecode) and on a pair known to be identical (any file against
itself). A result is only a pointer for a manual look, not a verdict.

The scan strips comments, strings and char literals before tokenizing, so it
measures code shape only; the comment check is separate on purpose.
"""
import glob
import os
import re
import sys


def tokens(text):
    text = re.sub(r"//[^\n]*", " ", text)
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    text = re.sub(r'"(?:[^"\\]|\\.)*"', " S ", text)
    text = re.sub(r"'(?:[^'\\]|\\.)'", " C ", text)
    return re.findall(r"[A-Za-z_][A-Za-z_0-9]*|[0-9]+|[^\sA-Za-z_0-9]", text)


def comments(text):
    out = []
    for m in re.finditer(r"//([^\n]*)", text):
        s = m.group(1).strip()
        if len(s) > 12:
            out.append(s)
    for m in re.finditer(r"/\*.*?\*/", text, flags=re.S):
        for line in m.group(1).splitlines():
            s = line.strip().lstrip("*").strip()
            if len(s) > 12:
                out.append(s)
    return out


def windows(tokens, n):
    return {tuple(tokens[i:i + n]) for i in range(len(tokens) - n + 1)}


def longest_run(a, b):
    """Longest contiguous shared token sequence, by binary search on length."""
    lo, hi, best = 1, min(len(a), len(b)), (0, None)
    while lo <= hi:
        mid = (lo + hi) // 2
        shared = windows(a, mid) & windows(b, mid)
        if shared:
            best = (mid, next(iter(shared)))
            lo = mid + 1
        else:
            hi = mid - 1
    return best


def jaccard(a, b, n=6):
    sa, sb = windows(a, n), windows(b, n)
    return len(sa & sb) / len(sa | sb) if sa and sb else 0.0


def compare(ours_path, theirs_path):
    ours_text = open(ours_path, encoding="utf-8", errors="replace").read()
    theirs_text = open(theirs_path, encoding="utf-8", errors="replace").read()
    a, b = tokens(ours_text), tokens(theirs_text)
    run, seq = longest_run(a, b)
    return {
        "run": run,
        "seq": seq,
        "jaccard": jaccard(a, b),
        "comments": len(set(comments(ours_text)) & set(comments(theirs_text))),
        "theirs": os.path.basename(theirs_path),
    }


def main():
    argv = sys.argv[1:]
    top = 5
    if "--top" in argv:
        i = argv.index("--top")
        top = int(argv[i + 1])
        del argv[i:i + 2]
    args = [a for a in argv if not a.startswith("--")]
    if len(args) != 2:
        raise SystemExit(__doc__)
    ours, theirs = args
    targets = ([theirs] if os.path.isfile(theirs)
               else sorted(glob.glob(os.path.join(theirs, "**", "*.rs"), recursive=True)))
    if not targets:
        raise SystemExit(f"no .rs files under {theirs}")
    rows = sorted((compare(ours, t) for t in targets), key=lambda r: -r["run"])
    print(f"{ours} vs {len(targets)} file(s)")
    print(f"{'maxrun':>6} {'jaccard':>8} {'comments':>8}  file")
    for r in rows[:top]:
        print(f"{r['run']:6} {r['jaccard']:8.4f} {r['comments']:8}  {r['theirs']}")
    worst = rows[0]
    if worst["seq"]:
        print("\nlongest shared run:\n  " + " ".join(worst["seq"])[:400])
    print(f"\nsummary: max run {rows[0]['run']}, max jaccard "
          f"{max(r['jaccard'] for r in rows):.4f}, "
          f"identical comment lines {sum(r['comments'] for r in rows)}")


if __name__ == "__main__":
    main()
