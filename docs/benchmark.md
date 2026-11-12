# Single-host contention benchmark

Build the release binary on the service host and the load generator host. Save a manifest from each host alongside the trial reports.

```sh
cargo build --release --locked
bin=./target/release/corereserve
"$bin" doctor --format json > lab-manifest.json
```

Use a separate client host for bare-metal measurements. For a local smoke run, pin the client to a separate physical core. The examples use CPUs 4 and 5 as the server pool and CPU 7 for the local client. Use `doctor` to select valid CPUs on your host and check that the static partition does not split SMT siblings between the service and batch. The benchmark uses Linux thread affinity. It requires no cgroup writes.

## Baselines

Run one service and, where applicable, one batch process per trial. Launch these in separate terminals on the service host. Wait for the service's JSON readiness line and, for trials with batch, the `batch ready` message. Keep both processes running throughout the client trial.

| Scenario | Service command | Batch command |
| --- | --- | --- |
| `service-only` | `"$bin" bench serve --cpus 4-5` | |
| `shared` | `"$bin" bench serve --cpus 4-5` | `"$bin" bench batch --cpus 4-5 --workers 2 --duration-secs 120 > shared-batch.json` |
| `static-partition` | `"$bin" bench serve --cpus 4` | `"$bin" bench batch --cpus 5 --workers 2 --duration-secs 120 > static-batch.json` |

For a remote client, pass the service host's lab-network address to `bench serve --listen` and use that same address as the client's `--target`. Use this synthetic TCP service on the lab network.

Run the client once per scenario with the corresponding label. Use the same request rate, duration, timeout, and in-flight cap across the comparison.

```sh
scenario=shared
"$bin" bench load --scenario "$scenario" --target 127.0.0.1:8400 \
  --cpus 7 --rate 500 --duration-secs 30 --timeout-ms 100 \
  > "$scenario-load.json"
```

Stop the service and batch processes after each client run with Ctrl-C or SIGTERM. Batch writes its report on exit. Save the service readiness JSON to record the effective mask, workload settings, and bound address. The batch throughput denominator is its own elapsed run time, which includes the startup lead and any delay before it is stopped. Keep those intervals short and consistent across trials.

## Read the results

- **Offered load** is the configured rate multiplied by the measurement duration. The client schedules arrivals from a monotonic clock independently of responses. Every scheduled arrival contributes to one outcome, including client capacity drops and arrivals that expire before admission.
- **Latency** runs from scheduled arrival to the validated response, including client admission delay and the TCP connection. Successful requests contribute to the histogram. Bucket keys are upper bounds in microseconds, rounded up to three significant digits. Percentiles use nearest rank. Timeouts, transport errors, invalid responses, and client drops are counted separately and included in the unsuccessful fraction.
- **Client saturation** is visible in the admission-lag histogram, pre-admission expirations, and capacity drops. Check these alongside the latency histogram when selecting an offered-load range.
- **Batch work** counts completed fixed-size CPU work units and reports units per second and per host-hour. Keep the iterations per unit and worker count fixed across scenarios. `--checkpoint PATH` saves progress every five seconds and on a graceful stop. Reuse the same worker count and iterations to resume. Each report counts new units for that invocation.

Pilot a range of offered loads with `--rate` and adjust the synthetic service cost with `bench serve --iterations` until the baseline exposes contention. Set the service SLO, error budget, and minimum sample count before repeated measurements. Run at least five trials per operating point, vary scenario order, and retain per-trial JSON. At p99.9, 1,000 successful requests give roughly one observation in the top 0.1%, so use longer trials when measuring that tail.
