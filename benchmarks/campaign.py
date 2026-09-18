#!/usr/bin/env python3
"""Run a repeated, sequential local Docker campaign. No CI or uploads."""
import argparse
import collections
import datetime as dt
import fcntl
import hashlib
import json
import os
from pathlib import Path
import statistics
import shutil
import subprocess
import sys
import time
import uuid

import run as bench

ROOT = bench.ROOT
PG_SETTINGS = {
    "shared_preload_libraries": "pg_textsearch,pg_search,pg_stat_statements",
    "shared_buffers": "1GB", "work_mem": "16MB", "maintenance_work_mem": "512MB",
    "max_parallel_workers": "4", "max_parallel_workers_per_gather": "2",
    "max_parallel_maintenance_workers": "2", "max_worker_processes": "8",
    "max_connections": "40", "jit": "off", "track_io_timing": "on",
    "synchronous_commit": "on", "fsync": "on", "full_page_writes": "on",
    "autovacuum": "on", "effective_cache_size": "3GB",
}


def docker(*args):
    return bench.command(["docker", *args])


def describe(values):
    ordered = sorted(values)
    if not ordered:
        return {"n": 0}
    mean = statistics.mean(ordered)
    return {"n": len(ordered), "values": values, "median": statistics.median(ordered),
            "trimmed_mean": statistics.mean(ordered[1:-1]) if len(ordered) >= 5 else None,
            "min": min(ordered), "max": max(ordered),
            "cv_percent": 100 * statistics.stdev(ordered) / mean if len(ordered) > 1 and mean else None}


def pressure(path):
    def fields(text):
        return {k: int(v) for k, v in (line.split() for line in (text or "").splitlines())}
    before = json.loads((path / "cgroup-before.json").read_text())
    after = json.loads((path / "cgroup-after.json").read_text())
    cpu_a, cpu_b = fields(before["cpu.stat"]), fields(after["cpu.stat"])
    mem_a, mem_b = fields(before["memory.events"]), fields(after["memory.events"])
    return {"cpu_throttled_seconds": (cpu_b.get("throttled_usec", 0) - cpu_a.get("throttled_usec", 0)) / 1e6,
            "memory_limit_events": mem_b.get("max", 0) - mem_a.get("max", 0),
            "oom_events": mem_b.get("oom", 0) - mem_a.get("oom", 0),
            "oom_kills": mem_b.get("oom_kill", 0) - mem_a.get("oom_kill", 0),
            "container_peak_bytes": int(after["memory.peak"]) if after["memory.peak"] else None}


def schedule(engines, profiles, repetitions):
    jobs = []
    for repetition in range(repetitions):
        # Rotate engine order; five rounds cannot perfectly balance four positions.
        for profile in profiles:
            available = [e for e in engines if profile == "count" or e != "gin"]
            offset = repetition % len(available) if available else 0
            for engine in available[offset:] + available[:offset]:
                jobs.append({"repetition": repetition + 1, "engine": engine, "profile": profile})
    return jobs


def aggregate(root):
    campaign = json.loads((root / "campaign.json").read_text())
    expected = campaign["config"]["repetitions"]
    groups = collections.defaultdict(list)
    invalid = []
    resource_pressure = {}
    for job in campaign["jobs"]:
        path = root / job["directory"]
        try:
            m = json.loads((path / "manifest.json").read_text())
            s = json.loads((path / "summary.json").read_text())
            if m["status"] != "complete":
                raise ValueError("incomplete run")
            resource_pressure[job["directory"]] = pressure(path)
            if resource_pressure[job["directory"]]["oom_events"] or resource_pressure[job["directory"]]["oom_kills"]:
                raise ValueError("OOM during timed traffic")
            groups[(job["engine"], job["profile"])].append((m, s))
        except (OSError, ValueError, KeyError) as error:
            invalid.append({"run": job["directory"], "error": str(error)})
    report = {"invalid_runs": invalid, "groups": {}, "resource_pressure": resource_pressure}
    lines = ["# Local campaign summary", "", "Each sample is one independent fresh-container run. All samples are retained.",
             "The trimmed mean removes one minimum and maximum per metric; it does not discard whole runs.",
             "Ranked/mixed rows describe each engine's workload, not equivalent relevance or cross-engine speedups.", "",
             "| Engine / workload | Runs | Read QPS median | Trimmed mean | Min–max | CV | Write QPS median |",
             "| --- | ---: | ---: | ---: | --- | ---: | ---: |"]
    for job in campaign["jobs"]:
        groups.setdefault((job["engine"], job["profile"]), [])
    for (engine, profile), runs in sorted(groups.items()):
        if not runs:
            lines.append(f"| {engine} / {profile} | 0/{expected} | incomplete | — | — | — | — |")
            continue
        reference = runs[0][0]
        # Seeds differ deliberately between repetitions. Everything else must agree.
        for manifest, _ in runs[1:]:
            a, b = json.loads(json.dumps(reference)), json.loads(json.dumps(manifest))
            a["config"].pop("seed")
            b["config"].pop("seed")
            if bench.comparison_mismatches(a, b):
                raise ValueError(f"Mismatched repetitions for {engine}/{profile}")
            if manifest["build_id"] != reference["build_id"] or manifest["source"]["source_sha256"] != reference["source"]["source_sha256"]:
                raise ValueError(f"Mixed engine builds within repetitions for {engine}/{profile}")
        if len(runs) != expected:
            lines.append(f"| {engine} / {profile} | {len(runs)}/{expected} | incomplete | — | — | — | — |")
            continue
        reads = describe([sum(q["completed_per_second"] for q in s["reader"]["queries"].values()) for _, s in runs])
        writes = describe([sum(q["completed_per_second"] for q in s.get("writer", {}).get("queries", {}).values()) for _, s in runs])
        queries = {}
        for name in reference["query_names"]:
            queries[name] = {}
            for metric in ("p50_ms", "p95_ms", "p99_ms", "completed_per_second"):
                vals = [s["reader"]["queries"][name][metric] for _, s in runs]
                queries[name][metric] = describe(vals) if all(v is not None for v in vals) else {"n": sum(v is not None for v in vals), "insufficient_samples": True}
        report["groups"][engine + "/" + profile] = {"read_qps": reads, "write_qps": writes, "queries": queries}
        trimmed = f"{reads['trimmed_mean']:.2f}" if reads["trimmed_mean"] is not None else "—"
        cv = f"{reads['cv_percent']:.1f}%" if reads["cv_percent"] is not None else "—"
        lines.append(f"| {engine} / {profile} | {len(runs)} | {reads['median']:.2f} | {trimmed} | {reads['min']:.2f}–{reads['max']:.2f} | {cv} | {writes['median']:.2f} |")
    lines += ["", "Per-query latency distributions and all run-level values: `aggregate.json`.",
              "Review write rates, resource pressure, failed runs and query plans before interpreting read throughput.",
              "Five runs provide a local baseline, not a significance test or a production-scale claim."]
    limit_events = sum(p["memory_limit_events"] for p in resource_pressure.values())
    throttled_runs = sum(p["cpu_throttled_seconds"] > 0 for p in resource_pressure.values())
    lines += ["", f"Resource observations: {limit_events} memory-limit events; {throttled_runs} runs with CPU-quota throttling.",
              "Throttling reflects the chosen CPU ceiling; raw counters are retained to distinguish it from other variation."]
    if invalid:
        lines += ["", f"**Incomplete campaign: {len(invalid)} missing/failed runs. They were not trimmed away.**"]
    bench.save(root / "aggregate.json", report)
    (root / "report.md").write_text("\n".join(lines) + "\n")


def start_server(image, name, volume, args):
    docker("volume", "create", volume)
    cmd = ["run", "-d", "--name", name, "--cpus", str(args.cpus), "--cpuset-cpus", args.cpuset,
           "--memory", args.memory, "--memory-swap", args.memory, "--shm-size", "1g",
           "-p", f"127.0.0.1:{args.port}:5432", "--mount", f"type=volume,src={volume},dst=/var/lib/postgresql",
           "-e", "POSTGRES_HOST_AUTH_METHOD=trust", "-e", "POSTGRES_DB=stannum_bench_campaign", image, "postgres"]
    for key, value in PG_SETTINGS.items():
        cmd += ["-c", key + "=" + value]
    docker(*cmd)
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        # Test forwarded TCP: entrypoint's temporary bootstrap server is socket-only.
        probe = subprocess.run(["pg_isready", "-h", "127.0.0.1", "-p", str(args.port), "-U", "postgres"], capture_output=True)
        if probe.returncode == 0:
            return
        state = json.loads(docker("inspect", name))[0]["State"]
        if not state["Running"]:
            raise RuntimeError("Benchmark server exited during startup")
        time.sleep(1)
    raise TimeoutError("Benchmark PostgreSQL did not become ready")


def execute(args):
    corpus = bench.dataset.verify(args.dataset, args.rows) if args.dataset else None
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    protocol = root / "protocol"
    protocol.mkdir()
    for name in ("run.py", "mutation.py", "campaign.py", "dataset.py", "Dockerfile", "Dockerfile.dockerignore"):
        shutil.copy2(Path(__file__).resolve().parent / name, protocol / name)
    source = json.loads(Path(args.source_manifest).read_text()) if args.source_manifest else bench.provenance(root)
    bench.save(root / "source.json", source)
    recipe = bench.digest((protocol / "Dockerfile").read_bytes() + (protocol / "Dockerfile.dockerignore").read_bytes())
    if args.build:
        with (root / "image-build.log").open("w") as log:
            subprocess.run(["docker", "build", "--platform", "linux/arm64", "-f", "benchmarks/Dockerfile",
                            "--build-arg", "STANNUM_SOURCE_SHA256=" + source["source_sha256"],
                            "--build-arg", "STANNUM_COMMIT=" + source["commit"],
                            "--build-arg", "RECIPE_SHA256=" + recipe, "-t", args.image, "."],
                           cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, check=True)
    image = json.loads(docker("image", "inspect", args.image))[0]
    if image["Architecture"] != "arm64":
        raise ValueError("This Mac campaign requires native ARM64, not emulated x86")
    if image["Config"].get("Labels", {}).get("benchmark.stannum_source_sha256") != source["source_sha256"]:
        raise ValueError("Image does not match engine source; rerun with --build")
    if image["Config"].get("Labels", {}).get("benchmark.recipe_sha256") != recipe:
        raise ValueError("Image does not match Docker build recipe; rerun with --build")
    image_id = image["Id"]
    info = json.loads(docker("info", "--format", "{{json .}}"))
    vm = {k: info.get(k) for k in ("NCPU", "MemTotal", "Architecture", "OperatingSystem", "ServerVersion", "KernelVersion")}
    context = {"protocol": "local-docker-v1", "platform": "linux/arm64", "vm": vm,
               "cpus": args.cpus, "cpuset": args.cpuset, "memory": args.memory,
               "swap": "disabled", "shm": "1g", "storage": "fresh named Docker volume per run",
               "postgres_settings": PG_SETTINGS, "client": "native macOS pgbench over loopback",
               "runner_sha256": bench.digest(Path(__file__).read_bytes()), "recipe_sha256": recipe}
    bench.save(root / "context.json", context)
    jobs = schedule(args.engines, args.profiles, args.repetitions)
    for job in jobs:
        job["directory"] = f"r{job['repetition']:02d}-{job['engine']}-{job['profile']}"
    manifest = {"status": "running", "started_at": dt.datetime.now(dt.timezone.utc).isoformat(),
                "source": source, "image_id": image_id, "image_repo_digests": image.get("RepoDigests", []),
                "config": {k: getattr(args, k) for k in ("rows", "seconds", "warmup", "clients", "write_rate", "repetitions", "seed", "engines", "profiles", "statement_timeout_ms")},
                "context": context, "jobs": jobs, "dataset": corpus}
    bench.save(root / "campaign.json", manifest)
    (root / "packages.txt").write_text(docker("run", "--rm", "--entrypoint", "cat", image_id, "/benchmark/packages.txt"))
    awake = subprocess.Popen(["caffeinate", "-i"]) if sys.platform == "darwin" else None
    try:
        for number, job in enumerate(jobs, 1):
            prefix = "stannum-campaign-" + uuid.uuid4().hex[:12]
            volume = prefix + "-data"
            path = root / job["directory"]
            job["status"] = "running"
            bench.save(root / "campaign.json", manifest)
            print(f"[{number}/{len(jobs)}] {job['directory']}", flush=True)
            (root / (job["directory"] + "-background.txt")).write_text(docker("stats", "--no-stream", "--format", "{{.Name}} {{.CPUPerc}} {{.MemUsage}}"))
            try:
                start_server(image_id, prefix, volume, args)
                inspect = json.loads(docker("inspect", prefix))[0]
                bench.save(root / (job["directory"] + "-container.json"),
                           {"image": inspect["Image"], "limits": {k: inspect["HostConfig"].get(k) for k in ("NanoCpus", "CpusetCpus", "Memory", "MemorySwap", "ShmSize")}})
                env = dict(os.environ, PGHOST="127.0.0.1", PGPORT=str(args.port), PGUSER="postgres")
                # No inherited tuning or libpq service file can silently change this campaign.
                for key in ("PGOPTIONS", "PGSERVICE", "PGSERVICEFILE", "PGDATABASE"):
                    env.pop(key, None)
                cmd = [sys.executable, str(protocol / "run.py"), "run", "--engine", job["engine"],
                       "--profile", job["profile"], "--database", "stannum_bench_campaign", "--output", str(path),
                       "--environment", "mac-studio-orbstack-" + bench.digest(bench.canonical(context))[:12],
                       "--build-id", image_id, "--source-manifest", str(root / "source.json"), "--context", str(root / "context.json"), "--container", prefix,
                       "--rows", str(args.rows), "--seconds", str(args.seconds), "--warmup", str(args.warmup),
                       "--clients", str(args.clients), "--write-rate", str(args.write_rate),
                       "--seed", str(args.seed + 1009 * (job["repetition"] - 1)), "--label", args.label]
                cmd += ["--statement-timeout-ms", str(args.statement_timeout_ms)]
                if args.dataset:
                    cmd += ["--dataset", str(Path(args.dataset).resolve())]
                with (root / (job["directory"] + "-runner.log")).open("w") as log:
                    subprocess.run(cmd, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
                job["status"] = "complete"
            except Exception as error:
                job["status"] = "failed"
                job["error"] = f"{type(error).__name__}: {error}"
                print(f"Failed {job['directory']}: {error}; continuing campaign", flush=True)
            finally:
                # Only remove resources created by this job, including on interruption.
                bench.save(root / (job["directory"] + "-final-cgroup.json"), bench.container_counters(prefix))
                logs = subprocess.run(["docker", "logs", prefix], capture_output=True, text=True)
                (root / (job["directory"] + "-server.log")).write_text(logs.stdout + logs.stderr)
                subprocess.run(["docker", "rm", "-f", prefix], capture_output=True)
                subprocess.run(["docker", "volume", "rm", volume], capture_output=True)
                bench.save(root / "campaign.json", manifest)
                aggregate(root)
        manifest["status"] = "complete" if all(j.get("status") == "complete" for j in jobs) else "incomplete"
    except BaseException:
        manifest["status"] = "failed"
        raise
    finally:
        if awake:
            awake.terminate()
            awake.wait()
        manifest["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        bench.save(root / "campaign.json", manifest)
        aggregate(root)
    print(root / "report.md", flush=True)
    return manifest["status"] == "complete"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--image", default="stannum-bench:local")
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--report-only", action="store_true")
    parser.add_argument("--label", default="local-baseline")
    parser.add_argument("--engines", nargs="+", choices=bench.ENGINES, default=["stannum", "gin", "paradedb", "pg_textsearch"])
    parser.add_argument("--profiles", nargs="+", choices=bench.PROFILES, default=["count", "mixed"])
    parser.add_argument("--repetitions", type=bench.positive, default=5)
    parser.add_argument("--source-manifest", help=argparse.SUPPRESS)
    parser.add_argument("--dataset", help="Verified dataset directory from dataset.py")
    parser.add_argument("--statement-timeout-ms", type=bench.positive, default=60000)
    parser.add_argument("--rows", type=bench.positive, default=10000)
    parser.add_argument("--seconds", type=bench.positive, default=30)
    parser.add_argument("--warmup", type=bench.positive, default=5)
    parser.add_argument("--clients", type=bench.positive, default=2)
    parser.add_argument("--write-rate", type=bench.positive, default=20)
    parser.add_argument("--seed", type=bench.positive, default=1729)
    parser.add_argument("--cpus", type=bench.positive, default=4)
    parser.add_argument("--cpuset", default="0-3")
    parser.add_argument("--memory", default="4g")
    parser.add_argument("--port", type=bench.positive, default=28918)
    args = parser.parse_args()
    if args.report_only:
        aggregate(Path(args.output).resolve())
        return
    if args.rows % 1000:
        parser.error("--rows must be a multiple of 1000")
    if len(set(args.engines)) != len(args.engines) or len(set(args.profiles)) != len(args.profiles):
        parser.error("duplicate engines/profiles are not allowed")
    if not schedule(args.engines, args.profiles, args.repetitions):
        parser.error("no supported engine/profile pairs selected")
    lock_path = ROOT / "benchmarks/results/.campaign.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if not execute(args):
            raise SystemExit(2)


if __name__ == "__main__":
    main()
