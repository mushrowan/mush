"""render a text summary of a mush perf bundle.

reads, if present:
  - stall-dumps/mush-stall-*.txt   tui watchdog dumps
  - mush-trace-*.json              chrome trace (draw reasons, span timings)
  - profile.json.gz                samply cpu profile (+ .syms.json sidecar)

every section degrades gracefully if its input is missing or truncated,
so a killed run still produces a useful summary.
"""

import bisect
import collections
import gzip
import json
import sys
from pathlib import Path


def section(title):
    print(title)


def render_stalls(data_dir):
    stall_dir = data_dir / "stall-dumps"
    stalls = sorted(stall_dir.glob("mush-stall-*.txt")) if stall_dir.exists() else []
    section("tui stall dumps (fire on any await over threshold, incl. idle waits)")
    if not stalls:
        print("  none found\n")
        return
    for path in stalls:
        fields = {"phase": "?", "detail": "", "elapsed_ms": "?"}
        for line in path.read_text(errors="replace").splitlines():
            for key in fields:
                if line.startswith(key + ": "):
                    fields[key] = line[len(key) + 2 :]
        print(f"  {path.name}: elapsed={fields['elapsed_ms']}ms "
              f"phase={fields['phase']} detail={fields['detail']}")
    print()


def render_trace(data_dir):
    trace = next(data_dir.glob("mush-trace-*.json"), None)
    if not trace or not trace.exists():
        section("chrome trace")
        print("  none found\n")
        return
    # the trace is a json array, one object per line; the final line may be
    # truncated if mush was killed, so parse line by line and skip a bad tail.
    draw_reasons = collections.Counter()
    stacks = collections.defaultdict(list)
    spans = collections.defaultdict(list)
    parsed = bad = 0
    for line in trace.open(errors="replace"):
        line = line.strip().rstrip(",")
        if not line or line[0] != "{":
            continue
        try:
            event = json.loads(line)
        except Exception:
            bad += 1
            continue
        parsed += 1
        name = event.get("name", "")
        cat = event.get("cat", "")
        args = event.get("args") or {}
        if name == "draw requested" or cat == "mush_tui::draw_reason":
            draw_reasons[args.get("reason", "unknown")] += 1
        phase = event.get("ph")
        tid = event.get("tid", 0)
        if phase == "B":
            stacks[tid].append((name, event.get("ts", 0.0)))
        elif phase == "E" and stacks[tid]:
            start_name, start_ts = stacks[tid].pop()
            spans[start_name].append(event.get("ts", 0.0) - start_ts)

    section(f"chrome trace (events parsed={parsed} unparsable={bad})")
    if draw_reasons:
        print("  draw reasons:")
        for reason, count in draw_reasons.most_common():
            print(f"    {count:8d} {reason}")
    print("  span timings (wall clock):")
    for name in ["draw_panes", "llm_stream", "tool_exec", "agent_turn"]:
        values = sorted(spans.get(name, []))
        if not values:
            continue
        count = len(values)
        total = sum(values) / 1000.0

        def q(frac):
            return values[min(count - 1, int(frac * count))] / 1000.0

        print(f"    {name:12} n={count:8d} total={total:10.1f}ms "
              f"p50={q(0.50):7.3f}ms p90={q(0.90):7.3f}ms "
              f"p99={q(0.99):7.3f}ms max={values[-1] / 1000.0:7.3f}ms")
    print()


def build_symbol_resolvers(profile, syms):
    # profile lib index -> fn(relative_address) -> symbol name, via the
    # samply presymbolicate sidecar (per-lib symbol_table of {rva, size}).
    string_table = syms["string_table"]
    by_name = {}
    for entry in syms["data"]:
        ordered = sorted(entry["symbol_table"], key=lambda s: s["rva"])
        starts = [s["rva"] for s in ordered]
        names = [string_table[s["symbol"]] for s in ordered]
        exact = {a: string_table[i] for a, i in entry.get("known_addresses", [])}
        by_name[entry["debug_name"]] = (starts, names, exact)

    resolvers = {}
    for index, lib in enumerate(profile["libs"]):
        key = lib.get("debugName") or lib.get("name")
        if key in by_name:
            starts, names, exact = by_name[key]

            def resolve(addr, starts=starts, names=names, exact=exact):
                if addr in exact:
                    return exact[addr]
                j = bisect.bisect_right(starts, addr) - 1
                return names[j] if j >= 0 else None

            resolvers[index] = resolve
    return resolvers


def render_cpu(data_dir):
    profile_path = data_dir / "profile.json.gz"
    if not profile_path.exists():
        return
    section("samply cpu profile")
    syms_path = data_dir / "profile.json.syms.json"
    if not syms_path.exists():
        print("  profile present but not symbolicated\n"
              "  open it with: nix run .#perf-load\n")
        return

    profile = json.loads(gzip.decompress(profile_path.read_bytes()))
    syms = json.loads(syms_path.read_text())
    resolvers = build_symbol_resolvers(profile, syms)

    self_time = collections.Counter()
    total = 0
    for thread in profile["threads"]:
        samples = thread["samples"]
        stack_tab = thread["stackTable"]
        frames = thread["frameTable"]
        funcs = thread["funcTable"]
        resources = thread["resourceTable"]
        strings = thread["stringArray"]
        weights = samples.get("weight") or [1] * samples["length"]

        def symbol(frame):
            func = frames["func"][frame]
            addr = frames["address"][frame]
            res = funcs["resource"][func]
            lib = resources["lib"][res] if res is not None and res >= 0 else None
            resolver = resolvers.get(lib)
            if resolver is not None and addr is not None and addr >= 0:
                got = resolver(addr)
                if got is not None:
                    return got
            return strings[funcs["name"][func]]

        for i in range(samples["length"]):
            stack = samples["stack"][i]
            if stack is None:
                continue
            weight = weights[i] or 1
            total += weight
            self_time[symbol(stack_tab["frame"][stack])] += weight

    if total == 0:
        print("  no samples\n")
        return
    print(f"  {total} samples, top self (leaf) functions:")
    for sym, weight in self_time.most_common(25):
        print(f"    {100 * weight / total:5.1f}%  {sym[:90]}")
    print("\n  (self time only; call graphs need frame pointers. interactive\n"
          "   flame graph: nix run .#perf-load)\n")


def main():
    data_dir = Path(sys.argv[1])
    print("mush profile summary")
    print(f"bundle: {data_dir}\n")
    render_stalls(data_dir)
    render_trace(data_dir)
    render_cpu(data_dir)


if __name__ == "__main__":
    main()
