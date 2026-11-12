# Dry-run host policy

Save the three load reports described in the [benchmark procedure](benchmark.md). The example trace uses the smoke-run settings of 750 requests per second for 3 seconds, a 100 ms timeout, and 200,000 service iterations. It places the completed windows on a simulated timeline and injects CPU pressure observations. Its 2-second recovery window and SLO limits are for exercising the transitions. Use measured limits and the build plan's 30-second initial recovery window when qualifying the policy.

```sh
reports=.
cp docs/examples/policy-trace.json "$reports/policy-trace.json"
cargo run --locked -- policy dry-run \
  --config docs/examples/policy.json --trace "$reports/policy-trace.json" \
  > "$reports/policy-audit.json"
```

Report paths are relative to the trace file. Each service observation binds a completed benchmark report to the configured service instance. `observed_at_ms` is when that observation became available on the trace's monotonic timeline. Tick times and each source's observation times must increase strictly. An omitted source retains its last observation until the configured maximum age. An empty tick evaluates freshness without supplying telemetry.

For recorded CPU pressure, derive `some_ppm` from the change in the service cgroup's `cpu.pressure` `some total` divided by the elapsed interval, with both measured in microseconds. Multiply by 1,000,000 and record the result as integer parts per million. [Linux PSI documentation](https://docs.kernel.org/accounting/psi.html)

The replay recomputes p99.9 from validated histogram buckets. It treats transport errors, protocol errors, and timeouts as service failures for the lab policy. Client capacity drops and pre-admission expirations make the service window unusable for admission. Configure separate minimum counts for offered requests and successful latency samples so a clear error-budget breach can reduce quota even when too few requests succeed for p99.9.

| State | Quota and admission |
| --- | --- |
| `healthy` | At the configured ceiling with batch admission enabled. |
| `constrained` | A pressure or service breach cuts quota to the floor. Pressure between the entry and exit thresholds holds quota and resets the recovery timer when recovering. Admission stays closed. |
| `recovering` | Fresh telemetry within limits and pressure at or below the exit threshold start the stable window. Each eligible tick can raise quota by one configured step. Admission opens when the ceiling is restored. |
| `unknown` | Missing, stale, or insufficient evidence closes admission and caps quota at the conservative ceiling. Existing lower quotas stay in effect. A fresh pressure or service breach can still cut quota to the floor. |

Each decision includes the policy revision, reason codes, the input evidence, and proposed quota and period in microseconds. These are the two values used by cgroup v2 `cpu.max`. The replay reads only its input files and prints the audit to stdout. [Linux cgroup v2 CPU controls](https://docs.kernel.org/admin-guide/cgroup-v2.html#cpu)
