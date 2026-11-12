# CoreReserve setup and usage

## 1. Set up a Linux host

Use an x86-64 Linux host with cgroup v2 and Rustup installed. Run these commands from the CoreReserve checkout containing `Cargo.toml`. Rustup uses the compiler version and components pinned in [`rust-toolchain.toml`](../rust-toolchain.toml).

```sh
cargo build --release --locked
./target/release/corereserve doctor
./target/release/corereserve doctor --format json > lab-manifest.json
```

`doctor` reports the kernel, CPU topology, available controllers, effective CPU mask, pressure support, and quota access. Save the JSON manifest with your benchmark results. Live enforcement requires a writable, delegated batch cgroup. The diagnostics, benchmark, and policy replay can run on a host with a read-only cgroup mount.

## 2. Run a smoke test

From the repository root, start the synthetic service in one terminal. Wait for the JSON line containing `"event":"service_ready"`.

```sh
./target/release/corereserve bench serve --listen 127.0.0.1:8400
```

In a second terminal at the same repository root, send a fixed offered load and save the report.

```sh
./target/release/corereserve bench load --scenario service-only \
  --target 127.0.0.1:8400 --rate 100 --duration-secs 5 --timeout-ms 100 \
  > service-only-load.json
```

The report contains 500 scheduled requests. Check `outcomes` for successes, timeouts, transport errors, and client drops. The `p999_us` field describes successful requests and is useful here as an output check. Use longer trials with enough successful requests to assess p99.9. Stop the service with Ctrl-C when you finish.

## 3. Use case: run batch work beside an online service

An operator has a latency-sensitive RPC service and an interruptible CPU batch workload on the same host. Service demand rises during bursts, so the operator wants to reduce batch CPU allocation during contention and restore it gradually after the service recovers. The lab uses a synthetic TCP service and a checkpointable CPU workload to exercise that behavior.

1. **Establish a baseline.** Run service-only, shared scheduling, and static CPU partitioning at the same offered load. Compare completed batch work, service latency, and failures to choose an operating point. [Benchmark procedure](benchmark.md)
2. **Replay the policy.** Edit a copy of [the example policy](examples/policy.json) with your latency limit, error budget, and quota bounds. Replay the recorded reports to check which observations cause a quota cut or allow recovery. [Policy replay](policy-replay.md)
3. **Observe the live agent.** Create separate service and batch cgroups through systemd delegation. Start the benchmark publisher on the agent's host and in the same time namespace. Run `host run` without `--apply` to inspect proposed quotas before enabling enforcement. [Delegated systemd lab](host-lab.md)
4. **Apply and verify.** Run `host run --apply` against the delegated batch cgroup. Use `host status` and the live audit to compare the proposed quota with the actual kernel setting. Stop publishing service windows to verify that the policy enters `unknown` after the configured freshness limit. Stop the agent before tearing down the lab cgroups. [Delegated systemd lab](host-lab.md#start-the-agent)

The sample policy uses a 100 ms quota period. Its 100,000 µs healthy ceiling represents one CPU of runtime, its 25,000 µs constrained floor represents a quarter CPU, and its 50,000 µs stale-data ceiling represents half a CPU. During recovery, the audit should show bounded increases. When telemetry becomes stale, the agent preserves any existing quota below the conservative ceiling. [Example policy](examples/policy.json)

## Read the live audit

| Field | What to check |
| --- | --- |
| `state` and `reasons` | Whether the agent saw healthy, constrained, recovering, or unknown conditions and why. |
| `proposed_quota_us` and `cpu_period_us` | The CPU bandwidth requested by the policy. |
| `actual_cpu_max` and `quota_verified` | The observed kernel limit and whether the actuator verified enforcement. |
| `batch_cpu_stat.nr_throttled` | The cumulative number of throttled periods. Compare readings before and after a quota change. |
| `effective_admit_batch` | The prototype's recorded admission decision. Batch processes are launched manually in the lab. |

For performance qualification, use a separate client host, freeze the latency and error objectives before repeated trials, and compare batch throughput against the strongest baseline that meets those objectives. [Benchmark procedure](benchmark.md#read-the-results)
