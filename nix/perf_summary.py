"""render a text summary of a mush perf bundle.

reads, if present:
  - stall-dumps/mush-stall-*.txt   tui watchdog dumps
  - mush-trace-*.json              chrome trace (draw reasons, span timings)
  - profile.json.gz                samply cpu profile (+ .syms.json sidecar)

every section degrades gracefully if its input is missing or truncated,
so a killed run still produces a useful summary.
"""

import collections
import gzip
import json
import os
import subprocess
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


def addr2line(binary, addresses, tool):
    """resolve library-relative addresses to demangled function names.

    samply's own symbolication (--unstable-presymbolicate sidecar) was
    observed to map addresses to entirely wrong symbols on this toolchain,
    so we go straight to the recorded binary with addr2line, which demangles
    rust v0 names with -C. the frame address is already the library-relative
    offset addr2line expects.
    """
    if not binary or not addresses or not os.path.exists(binary):
        return {}
    stdin = "\n".join(f"0x{a:x}" for a in addresses)
    try:
        proc = subprocess.run(
            [tool, "-f", "-C", "-e", binary],
            input=stdin,
            capture_output=True,
            text=True,
            timeout=180,
        )
    except Exception:
        return {}
    lines = proc.stdout.splitlines()
    # addr2line emits two lines per address: the function then file:line
    return {
        addr: lines[2 * i].strip()
        for i, addr in enumerate(addresses)
        if 2 * i < len(lines)
    }


def render_cpu(data_dir):
    profile_path = data_dir / "profile.json.gz"
    if not profile_path.exists():
        return
    section("samply cpu profile")
    profile = json.loads(gzip.decompress(profile_path.read_bytes()))
    libs = profile["libs"]
    tool = os.environ.get("MUSH_ADDR2LINE", "addr2line")

    # accumulate cpu-weighted self time per (lib index, leaf address)
    weight_by_addr = collections.defaultdict(float)
    total = 0.0
    for thread in profile["threads"]:
        samples = thread["samples"]
        stack_tab = thread["stackTable"]
        frames = thread["frameTable"]
        funcs = thread["funcTable"]
        resources = thread["resourceTable"]
        # weight by cpu time (downweights threads parked in epoll); fall back
        # to per-sample weight, then to a flat count
        cpu = samples.get("threadCPUDelta")
        weight_col = samples.get("weight")
        for i in range(samples["length"]):
            stack = samples["stack"][i]
            if stack is None:
                continue
            if cpu and cpu[i] is not None:
                weight = cpu[i]
            elif weight_col and weight_col[i] is not None:
                weight = weight_col[i]
            else:
                weight = 1
            if weight <= 0:
                continue
            total += weight
            frame = stack_tab["frame"][stack]
            func = frames["func"][frame]
            addr = frames["address"][frame]
            res = funcs["resource"][func]
            lib = resources["lib"][res] if res is not None and res >= 0 else None
            weight_by_addr[(lib, addr)] += weight

    if total == 0:
        print("  no samples\n")
        return

    # batch addr2line per binary
    addrs_by_lib = collections.defaultdict(list)
    for lib, addr in weight_by_addr:
        if lib is not None and addr is not None and addr >= 0:
            addrs_by_lib[lib].append(addr)
    resolved = {}
    for lib, addrs in addrs_by_lib.items():
        info = libs[lib] if 0 <= lib < len(libs) else {}
        names = addr2line(info.get("path"), addrs, tool)
        libname = info.get("name", "?")
        for addr in addrs:
            name = names.get(addr) or ""
            if not name or name.startswith("??"):
                name = f"{libname}+{addr:#x}"
            resolved[(lib, addr)] = name

    self_time = collections.Counter()
    for key, weight in weight_by_addr.items():
        self_time[resolved.get(key, f"unknown+{key[1]:#x}")] += weight

    print(f"  cpu-weighted samples: {total:.0f}; symbolicated via {os.path.basename(tool)}")
    print("  top self (leaf) functions:")
    for sym, weight in self_time.most_common(25):
        print(f"    {100 * weight / total:5.1f}%  {clip(sym)}")
    # self time is the sampled instruction pointer, so it needs no stack
    # unwinding and is the reliable, llm-readable hotspot view. call graphs
    # are not summarised: samply's unwinder degrades across precompiled
    # std/libc frames (no frame pointers) and flattens inline frames, so an
    # automated tree would mislead. use the flame graph for call context.
    print("\n  caller context / call graph: nix run .#perf-load\n")


def clip(sym, width=88):
    return sym if len(sym) <= width else sym[: width - 1] + "\u2026"



def main():
    data_dir = Path(sys.argv[1])
    print("mush profile summary")
    print(f"bundle: {data_dir}\n")
    render_stalls(data_dir)
    render_trace(data_dir)
    render_cpu(data_dir)


if __name__ == "__main__":
    main()
