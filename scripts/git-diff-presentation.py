#!/usr/bin/env python3
"""Summarize the Wayland presentation feedback from the Git image scroll benchmark."""

import argparse
from pathlib import Path
import statistics


CASES = ("cold", "warm", "reverse", "split")


def read_cases(path):
    case = None
    samples = []
    discarded = 0
    with path.open() as log:
        for line in log:
            parts = line.split()
            if not parts:
                continue
            event, *tokens = parts
            fields = dict(token.split("=", 1) for token in tokens if "=" in token)
            if event == "diff_scroll_begin":
                case = fields["case"]
                samples = []
                discarded = 0
            elif event == "wayland_presentation" and case is not None:
                if "discarded" in tokens:
                    discarded += 1
                else:
                    samples.append((int(fields["presented_ns"]), int(fields["refresh_ns"])))
            elif event == "diff_scroll_end" and case is not None and case == fields["case"]:
                yield case, samples, discarded
                case = None


def summarize(path):
    passed = True
    seen = set()
    for case, samples, discarded in read_cases(path):
        seen.add(case)
        intervals = [b[0] - a[0] for a, b in zip(samples, samples[1:]) if b[0] > a[0]]
        refreshes = [refresh for _, refresh in samples if refresh > 0]
        if not intervals or not refreshes:
            print(f"{path.name} {case}: no presentation cadence available")
            passed = False
            continue
        refresh = statistics.median(refreshes)
        duration = sum(intervals) / 1e9
        hz = 1e9 / refresh
        missed = [max(0, round(delta / refresh) - 1) for delta in intervals]
        on_time = len(intervals) / (len(intervals) + sum(missed))
        longest_miss = max(missed)
        meets_budget = (
            duration >= 10
            and 59.5 <= hz <= 60.5
            and on_time >= 0.99
            and longest_miss <= 1
        )
        passed &= meets_budget
        result = "pass" if meets_budget else "fail"
        print(
            f"{path.name} {case}: {result}, {duration:.2f} s at {hz:.2f} Hz, "
            f"{on_time:.2%} refreshes presented, {longest_miss} consecutive missed, "
            f"{discarded} discarded"
        )
    missing = set(CASES) - seen
    if missing:
        print(f"{path.name}: missing completed cases {', '.join(sorted(missing))}")
    return passed and not missing


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", type=Path, nargs="+")
    outcomes = [summarize(path) for path in parser.parse_args().logs]
    return 0 if all(outcomes) else 1


if __name__ == "__main__":
    raise SystemExit(main())
