#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Interleaved A/B of images on a saved database, counted in instructions.

    vm_ab.py --data COPY_OF_DB --queries queries.json --out DIR [options] IMAGE...

On a Mac, a backend's CPU time inside the Docker VM swings by about 10%
between runs with the same binary, with the cores and clock the VM's vCPUs
happen to get, so CPU milliseconds per query cannot resolve a few percent.
The instructions the VM retires can: the host counts them for the VM
process (`proc_pid_rusage`, RUSAGE_INFO_V4). This script measures those.

For every round, every image in turn (the order rotated each round, the
server restarted for each image) serves CLIENTS connections taking the
benchmark driver's shuffled (record, style) entries from one shared
counter. First the mixed trace runs, then each style alone. Over each
measured window, the VM's counters are read every SLICE seconds, and each
slice's instructions and cycles are divided by the queries that finished in
it. The result per window is the median over its slices. The backends'
CPU milliseconds per query (from /proc/self/schedstat) are reported beside
them.

The VM's counters include everything else the VM runs. Every other
container using more than 2% CPU during a window is listed with it. Keep the
VM otherwise quiet; a 150M server should hold the machine's 150M lock (see
README). Use an identical binary as one of the arms to see the noise floor.

--data is mounted at /var/lib/postgresql as the benchmark's volume is; give
it a COPY of a saved database (`tin.py run --save-database`), since the
server writes to it. --queries is the published queries.json. --records
keeps only the first N records (for example, to evaluate a profile on
records it was not trained on). Writes DIR/<arm>-r<round>-<phase>.json and
DIR/summary.txt. Needs psycopg 3 (STANNUM_PYTHON).
"""
import argparse
import ctypes
import json
import os
import secrets
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

STYLES = ("conjunction", "disjunction", "phrase")
SQL = ("SELECT id, body, stannum.score(ctid) AS score FROM documents "
       "WHERE body ==> %s ORDER BY score DESC LIMIT 10")
SCHED = "SELECT pg_read_file('/proc/self/schedstat')"
CONTAINER = "stannum-vm-ab"

_RUSAGE = ["user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins", "wired_size",
           "resident_size", "phys_footprint", "proc_start_abstime", "proc_exit_abstime",
           "child_user_time", "child_system_time", "child_pkg_idle_wkups", "child_interrupt_wkups",
           "child_pageins", "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten",
           "cpu_time_qos_default", "cpu_time_qos_maintenance", "cpu_time_qos_background",
           "cpu_time_qos_utility", "cpu_time_qos_legacy", "cpu_time_qos_user_initiated",
           "cpu_time_qos_user_interactive", "billed_system_time", "serviced_system_time",
           "logical_writes", "lifetime_max_phys_footprint", "instructions", "cycles",
           "billed_energy", "serviced_energy", "interval_max_phys_footprint", "runnable_time"]


class _RusageV4(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(f, ctypes.c_uint64) for f in _RUSAGE]


def vm_counters(pid):
    """Instructions and cycles a host process (the VM) has retired so far."""
    info = _RusageV4()
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    if libproc.proc_pid_rusage(int(pid), 4, ctypes.byref(info)) != 0:
        raise OSError(ctypes.get_errno(), f"proc_pid_rusage({pid})")
    return time.time(), info.instructions, info.cycles


def vm_pid():
    for pattern in ("OrbStack Helper.app/Contents/MacOS/OrbStack Helper",
                    "com.apple.Virtualization.VirtualMachine"):
        found = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True).stdout.split()
        if found:
            return int(found[0])
    sys.exit("no VM process found (OrbStack or Docker Desktop); pass --vm-pid")


def bench_order(records, seed=1592614637):
    """The benchmark driver's order (selectQueries in queries.js): every
    (record, style) pair, shuffled by its LCG."""
    entries = [(r["engines"]["tin"][s], s) for r in records for s in STYLES]
    state = seed & 0xffffffff
    for i in range(len(entries) - 1, 0, -1):
        state = (1664525 * state + 1013904223) & 0xffffffff
        j = int((state / 0x100000000) * (i + 1))
        entries[i], entries[j] = entries[j], entries[i]
    return entries


def connect(port, password):
    import psycopg
    return psycopg.connect(host="127.0.0.1", port=port, user="postgres", dbname="benchmark",
                           password=password, autocommit=True)


def start_server(args, image, password):
    subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)
    for pid in Path(args.data).glob("*/docker/postmaster.pid"):
        pid.unlink()
    subprocess.run(["docker", "run", "-d", "--name", CONTAINER, "--cpus", str(args.cpus),
                    "--memory", args.memory, "--memory-swap", args.memory, "--shm-size", "1g",
                    "-p", f"127.0.0.1:{args.port}:5432", "-v", f"{args.data}:/var/lib/postgresql",
                    "-e", "POSTGRES_PASSWORD", image, "postgres", "-c", f"shared_buffers={args.shared_buffers}",
                    "-c", "work_mem=16MB", "-c", "jit=off", "-c", "autovacuum=off"],
                   check=True, capture_output=True, env=dict(os.environ, POSTGRES_PASSWORD=password))
    while subprocess.run(["docker", "exec", CONTAINER, "pg_isready", "-U", "postgres"],
                         capture_output=True).returncode:
        time.sleep(2)
    # The saved database keeps its own password; set this run's.
    subprocess.run(["docker", "exec", "-i", CONTAINER, "psql", "-qU", "postgres", "-d", "benchmark"],
                   input=f"ALTER USER postgres PASSWORD '{password}';\n", text=True, check=True)


def busy_containers():
    out = subprocess.run(["docker", "stats", "--no-stream", "--format", "{{.Name}} {{.CPUPerc}}"],
                         capture_output=True, text=True).stdout
    busy = []
    for line in out.splitlines():
        name, _, cpu = line.rpartition(" ")
        if name != CONTAINER and float(cpu.rstrip("%") or 0) > 2:
            busy.append(f"{name} {cpu}")
    return busy


def window(args, order, password, warm, measure, pid):
    """CLIENTS connections over `order` for warm + measure seconds; per-query
    finish times, styles and backend CPU in the measured part; VM counter
    samples every SLICE seconds across it."""
    lock = threading.Lock()
    nxt = [0]
    t_measure = time.time() + warm
    t_end = t_measure + measure
    rows = []

    def client():
        conn = connect(args.port, password)
        cur = conn.cursor()
        cur.execute(SCHED)
        cpu0 = int(cur.fetchone()[0].split()[0])
        mine = []
        while time.time() < t_end:
            with lock:
                n = nxt[0]
                nxt[0] += 1
            text, style = order[n % len(order)]
            t = time.time()
            cur.execute(SQL, (text,))
            cur.fetchall()
            cur.execute(SCHED)
            cpu1 = int(cur.fetchone()[0].split()[0])
            done = time.time()
            if t >= t_measure:
                mine.append((done, style, (cpu1 - cpu0) / 1e6))
            cpu0 = cpu1
        conn.close()
        with lock:
            rows.extend(mine)

    threads = [threading.Thread(target=client) for _ in range(args.clients)]
    for t in threads:
        t.start()
    time.sleep(max(0.0, t_measure - time.time()))
    samples = [vm_counters(pid)]
    while samples[-1][0] < t_end - 0.5:
        time.sleep(max(0.0, min(args.slice, t_end - time.time())))
        samples.append(vm_counters(pid))
    for t in threads:
        t.join()
    slices = []
    for (t0, i0, c0), (t1, i1, c1) in zip(samples, samples[1:]):
        n = sum(1 for r in rows if t0 <= r[0] < t1)
        if n:
            slices.append(dict(seconds=t1 - t0, queries=n, ins=(i1 - i0) / n, cyc=(c1 - c0) / n))
    styles = {s: [r[2] for r in rows if r[1] == s] for s in STYLES}
    return dict(qps=len(rows) / measure, queries=len(rows), slices=slices,
                ins_per_q=statistics.median(s["ins"] for s in slices) if slices else None,
                cyc_per_q=statistics.median(s["cyc"] for s in slices) if slices else None,
                cpu_ms=sum(r[2] for r in rows) / max(len(rows), 1),
                cpu_ms_by_style={s: sum(v) / len(v) for s, v in styles.items() if v},
                busy=busy_containers())


def summarize(results, names):
    lines = []
    phases = ["mixed", *STYLES]
    for phase in phases:
        lines.append(f"== {phase}: median M instructions / M cycles per query, backend CPU ms per query")
        base = None
        for arm, name in enumerate(names):
            runs = [r for (a, _, p), r in sorted(results.items()) if a == arm and p == phase]
            if not runs:
                continue
            ins = statistics.median(s["ins"] for r in runs for s in r["slices"]) / 1e6
            cyc = statistics.median(s["cyc"] for r in runs for s in r["slices"]) / 1e6
            cpu = statistics.mean(r["cpu_ms"] for r in runs)
            line = f"  {name:<24} ins {ins:8.2f}  cyc {cyc:7.2f}  cpu {cpu:6.2f}"
            if base is None:
                base = (ins, cyc, cpu)
            else:
                line += (f"   vs {names[0]}: ins {(ins / base[0] - 1) * 100:+5.1f}%"
                         f"  cyc {(cyc / base[1] - 1) * 100:+5.1f}%  cpu {(cpu / base[2] - 1) * 100:+5.1f}%")
            lines.append(line)
            for r in runs:
                if r["busy"]:
                    lines.append(f"      busy in a window: {', '.join(r['busy'])}")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("images", nargs="+")
    parser.add_argument("--data", type=Path, required=True)
    parser.add_argument("--queries", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--records", type=int)
    parser.add_argument("--clients", type=int, default=8)
    parser.add_argument("--warm", type=float, default=90, help="seconds before the mixed window")
    parser.add_argument("--measure", type=float, default=60, help="the mixed window")
    parser.add_argument("--style-warm", type=float, default=15)
    parser.add_argument("--style-measure", type=float, default=45)
    parser.add_argument("--slice", type=float, default=15)
    parser.add_argument("--port", type=int, default=55493)
    parser.add_argument("--cpus", type=int, default=8)
    parser.add_argument("--memory", default="32g")
    parser.add_argument("--shared-buffers", default="24GB")
    parser.add_argument("--vm-pid", type=int)
    args = parser.parse_args()
    args.data = args.data.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    pid = args.vm_pid or vm_pid()
    records = json.loads(args.queries.read_text())["queries"][:args.records]
    mixed = bench_order(records)
    password = secrets.token_hex(16)
    results = {}
    try:
        for rnd in range(1, args.rounds + 1):
            for k in range(len(args.images)):
                arm = (k + rnd - 1) % len(args.images)
                start_server(args, args.images[arm], password)
                print(f"== {args.images[arm]} round {rnd} {time.strftime('%H:%M:%S')}", flush=True)
                for phase in ("mixed", *STYLES):
                    order = mixed if phase == "mixed" else [e for e in mixed if e[1] == phase]
                    warm, measure = ((args.warm, args.measure) if phase == "mixed"
                                     else (args.style_warm, args.style_measure))
                    result = window(args, order, password, warm, measure, pid)
                    results[(arm, rnd, phase)] = result
                    (args.out / f"arm{arm}-r{rnd}-{phase}.json").write_text(json.dumps(
                        dict(image=args.images[arm], round=rnd, phase=phase, **result), indent=1))
                    print(f"   {phase:<12} {result['qps']:6.1f} qps  "
                          f"{(result['ins_per_q'] or 0) / 1e6:8.2f} M ins/q  {result['cpu_ms']:6.2f} cpu ms/q"
                          + (f"  busy: {', '.join(result['busy'])}" if result["busy"] else ""), flush=True)
    finally:
        subprocess.run(["docker", "stop", "-t", "60", CONTAINER], capture_output=True)
        subprocess.run(["docker", "rm", CONTAINER], capture_output=True)
    text = summarize(results, args.images)
    (args.out / "summary.txt").write_text(text + "\n")
    print(text)


if __name__ == "__main__":
    main()
