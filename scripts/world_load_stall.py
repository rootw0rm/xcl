#!/usr/bin/env python3
"""Summarize the final sampled pause in presents from one saved XCL runtime log."""

import argparse
import re
from dataclasses import dataclass
from pathlib import Path


LOG_LINE = re.compile(r"^\[\s*(\d+\.\d{3})\]\[\s*\d+\]\[[A-Z ]{5}\] (.*)")
ACTIVITY = re.compile(
    r"activity \(2 s\): (\d+) presents, (\d+) submits, (\d+) draws, "
    r"(\d+) dispatches, .*; DirectStorage (\d+) requests,"
)
COMPILE_START = re.compile(r"D3D12\.X: driver compile start #(\d+): (.+?) PSO, (\d+) B DXIL")
COMPILE_FINISH = re.compile(r"D3D12\.X: driver compile finish #(\d+): ")
THREAD_DUMP = re.compile(r"thread dump \((\d+) threads\):")
THREAD_ROW = re.compile(r"thread (\d+) '.*': (.*)")
MODULE_FRAME = re.compile(r"(.+)\+(0x[0-9a-f]+)", re.IGNORECASE)


@dataclass(frozen=True)
class Activity:
    time: float
    presents: int
    submits: int
    draws: int
    dispatches: int
    requests: int


@dataclass(frozen=True)
class Compile:
    time: float
    kind: str
    dxil_bytes: int


def overlap_seconds(intervals):
    """Measure the union so concurrent compiles contribute only once."""
    total = 0.0
    covered_end = 0.0
    for start, end in sorted(intervals):
        total += max(0, end - max(start, covered_end))
        covered_end = max(covered_end, end)
    return total


def event_count(times, start, end):
    in_window = [time for time in times if start < time <= end]
    return f"{len(in_window)} (first at {min(in_window):.3f} s)" if in_window else "0"


def thread_dump_report(dumps, start, end, title_exe):
    latest = max(
        (dump for dump in reversed(dumps) if start < dump[0] <= end),
        key=lambda dump: dump[0],
        default=None,
    )
    if latest is None:
        return ["No timestamped thread dump in the sampled window."]

    time, expected, rows = latest
    title_exe = title_exe.casefold()
    visible_title = 0
    candidates = []
    for thread_id, stack in rows:
        offsets = []
        for entry in stack.split(" < "):
            frame = MODULE_FRAME.fullmatch(entry)
            if not frame:
                continue
            module, offset = frame.groups()
            module = module.casefold()
            if module == title_exe:
                visible_title += 1
                if offsets:
                    candidates.append(f"  Thread {thread_id}: {' < '.join(offsets)}")
                break
            if module == "xcl.exe":
                offsets.append(offset.lower())

    completeness = " (incomplete dump)" if len(rows) < expected else ""
    return [
        f"Thread dump at {time:.3f} s: {len(rows)}/{expected} thread lines parsed, "
        f"{visible_title} with a visible title frame{completeness}",
        f"Sampled title-thread candidates: {len(candidates)}",
        *candidates,
    ]


def summarize(lines, *, title_exe=None):
    samples = []
    starts = {}
    finishes = {}
    bridges = []
    releases = []
    dumps = []
    dump_rows = None
    stop = float("inf")
    for line in lines:
        match = LOG_LINE.match(line)
        if not match:
            # The logger writes a dump header and its unprefixed rows as one record.
            if dump_rows is not None:
                row = THREAD_ROW.fullmatch(line.rstrip("\r\n")) if line.endswith("\n") else None
                if row:
                    dump_rows.append(row.groups())
                if not row or len(dump_rows) == dumps[-1][1]:
                    dump_rows = None
            continue
        dump_rows = None
        time = float(match[1])
        message = match[2]
        if title_exe is not None and (header := THREAD_DUMP.fullmatch(message.rstrip("\r"))):
            expected = int(header[1])
            rows = []
            dumps.append((time, expected, rows))
            dump_rows = rows if expected else None
        elif activity := ACTIVITY.match(message):
            samples.append(Activity(time, *map(int, activity.groups())))
        elif compile_start := COMPILE_START.match(message):
            starts[int(compile_start[1])] = Compile(time, compile_start[2], int(compile_start[3]))
        elif finish := COMPILE_FINISH.match(message):
            finishes[int(finish[1])] = time
        elif message.startswith("D3D12.X: Wait64BitValueX at ") and " bridged to GPU queue.Wait " in message:
            bridges.append(time)
        elif (message.startswith("D3D12.X: GPU queue.Wait at ")
              and " released on CPU to prevent GPU hang" in message):
            releases.append(time)
        elif message.startswith("XCL_RUN_SECONDS: ") and " ending the title " in message:
            stop = min(stop, time)

    # Log timestamps are taken before the file lock. Use them to align threads.
    samples = sorted((sample for sample in samples if sample.time < stop), key=lambda sample: sample.time)
    if not samples:
        return "No activity samples found. Capture with XCL_ACTIVITY=1."
    last_present = next((i for i in reversed(range(len(samples))) if samples[i].presents), None)
    if last_present is None:
        return "Activity samples contain no presents. No presentation-loss window can be measured."
    last_active = samples[last_present]
    tail = samples[last_present + 1:]
    if not tail:
        return f"No terminal zero-present activity samples. Last sample with presents: {last_active.time:.3f} s."

    start = last_active.time
    end = tail[-1].time
    matches = []
    unmatched = []
    intersecting = []
    for number, build in starts.items():
        finish = finishes.get(number)
        left = max(start, build.time)
        right = min(end, finish) if finish is not None else end
        if right <= left:
            continue
        (matches if finish is not None else unmatched).append((left, right))
        intersecting.append(number)

    began_earlier = sum(starts[number].time < start for number in intersecting)
    report = [
        f"Last sample with presents: {start:.3f} s ({last_active.presents} presents)",
        f"Terminal zero-present samples: {len(tail)} (first at {tail[0].time:.3f} s)",
        f"Sampled window: {start:.3f} to {end:.3f} s ({end - start:.3f} s)",
        f"Sampled activity: {sum(sample.submits for sample in tail)} submits, "
        f"{sum(sample.draws for sample in tail)} draws, "
        f"{sum(sample.dispatches for sample in tail)} dispatches, "
        f"{sum(sample.requests for sample in tail)} DirectStorage requests",
        f"Logged driver compiles overlapping the window: {len(intersecting)} ({began_earlier} began earlier)",
        f"Matched driver compile overlap: {overlap_seconds(matches):.3f} s",
    ]
    if unmatched:
        report.append(
            f"Possible overlap including {len(unmatched)} "
            f"{'start' if len(unmatched) == 1 else 'starts'} without a logged finish: "
            f"{overlap_seconds(matches + unmatched):.3f} s (through the last sample)"
        )
    if intersecting:
        number = min(intersecting, key=lambda number: (starts[number].time, number))
        build = starts[number]
        report.append(
            f"First overlapping compile: #{number} {build.kind} ({build.dxil_bytes} B DXIL) at {build.time:.3f} s"
        )
    report.extend([
        f"Logged wait-before-signal bridges: {event_count(bridges, start, end)}",
        f"Logged GPU wait watchdog releases: {event_count(releases, start, end)}",
    ])
    if title_exe is not None:
        report.extend(thread_dump_report(dumps, start, end, title_exe))
    return "\n".join(report)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path, help="path to one saved xcl.log")
    parser.add_argument("--title-exe", metavar="NAME", help="title executable basename for thread dump analysis")
    args = parser.parse_args()
    if args.title_exe is not None and not re.fullmatch(r"[^\\/:\r\n]+\.exe", args.title_exe, re.IGNORECASE):
        parser.error("--title-exe requires an executable basename")
    try:
        with args.log.open(encoding="utf-8-sig", errors="replace") as log:
            print(summarize(log, title_exe=args.title_exe))
    except OSError as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
