# Live cgroup v2 lab

Use a Linux host with a writable cgroup v2 hierarchy and a user systemd manager that can delegate the CPU controller. CoreReserve reads service and batch `cpu.stat`, computes service CPU pressure from the change in the PSI `some total` counter, and reads back each change to the batch `cpu.max`. The service, batch, and agent run in separate cgroups so the batch quota cannot throttle the service or agent. [Linux CPU controller documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html#cpu) · [Linux PSI documentation](https://docs.kernel.org/accounting/psi.html)

## Create a disposable delegation

From the repository, build the binary and start an interactive shell under a temporary user unit. The parent cgroup can enable the CPU controller for its children because systemd puts the shell in the `supervisor` child. [systemd delegation](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html#DelegateSubgroup=)

```sh
cargo build --release --locked
systemd-run --user --unit="corereserve-lab-$$" --pty --wait --collect \
  --property=Delegate=cpu --property=DelegateSubgroup=supervisor \
  --setenv="CORERESERVE_BIN=$PWD/target/release/corereserve" \
  --setenv="CORERESERVE_CONFIG=$PWD/docs/examples/policy.json" \
  /bin/bash --noprofile --norc
```

Run the following inside that shell. The two children hold only the lab workloads. Use the readiness messages to confirm each workload started.

```bash
bin=$CORERESERVE_BIN
config=$CORERESERVE_CONFIG
member=$(sed -n 's/^0:://p' /proc/self/cgroup)
[[ $member == */corereserve-lab-*.service/supervisor ]] || exit 1
group=/sys/fs/cgroup${member%/supervisor}
printf '+cpu\n' > "$group/cgroup.subtree_control" || exit 1
mkdir "$group/service" "$group/batch" || exit 1
service=$group/service
batch=$group/batch
lab=$(mktemp -d)

in_cgroup() (
  printf '%s\n' "$BASHPID" > "$1/cgroup.procs" || exit 1
  shift
  exec "$@"
)

in_cgroup "$service" "$bin" bench serve --listen 127.0.0.1:8400 &
in_cgroup "$batch" "$bin" bench batch --workers 2 --duration-secs 300 &
"$bin" host status --service-cgroup "$service" --batch-cgroup "$batch"
```

## Start the agent

Use the same group paths and policy in the post-stop command. systemd calls it after an unexpected process exit, including a forced termination. The guard reads the current quota and applies the conservative ceiling when it would tighten the limit. [systemd service stop hooks](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html#ExecStopPost=)

```bash
agent_unit=corereserve-agent-lab-$$.service
systemd-run --user --unit="$agent_unit" --collect \
  -p Restart=on-failure -p RestartSec=1s \
  -p "ExecStopPost=\"$bin\" host cap --config \"$config\" --service-cgroup \"$service\" --batch-cgroup \"$batch\"" \
  "$bin" host run --apply --config "$config" \
  --service-cgroup "$service" --batch-cgroup "$batch" \
  --telemetry-file "$lab/service-window.json"
```

Run the load generator from the same host and time namespace. Repeat consecutive windows to exercise recovery, then stop publishing to exercise the stale-data ceiling. The sample configuration uses a short recovery window for lab verification.

```bash
for trial in 1 2 3 4; do
  "$bin" bench load --scenario shared --rate 500 --duration-secs 3 \
    --telemetry-file "$lab/service-window.json" --service-instance lab-rpc \
    > "$lab/load-$trial.json"
done
journalctl --user -u "$agent_unit" -o cat
"$bin" host status --service-cgroup "$service" --batch-cgroup "$batch"
systemctl --user stop "$agent_unit"
exit
```

The publisher atomically replaces the latest completed service window. Use a single publisher per file. Boot and time namespace identity bind the timestamp to the local agent's monotonic clock. Reports for a separate client host can still be used with [policy replay](policy-replay.md).

Each live audit line records the policy decision, actual quota, whether enforcement was verified, both cgroups' accounting, and any errors. A write failure closes effective admission and triggers a conservative reconciliation attempt. The agent retries on later samples and starts recovery again after reconciling. If the kernel continues rejecting quota writes, the prior quota can remain in effect. Startup and graceful shutdown also cap the quota without widening a tighter existing limit. The post-stop guard depends on systemd running the hook with continued write access to the same batch cgroup. A hung agent needs external supervision to trigger the hook.

Omit `--apply` for live observation without cgroup writes. `effective_admit_batch` is an audit result for this prototype. The lab launches its batch workload directly.
