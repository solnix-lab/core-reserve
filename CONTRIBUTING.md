# Contributing to CoreReserve

Benchmark results, bug reports, documentation, and Rust patches are welcome.
Check the [open issues](https://github.com/solnix-lab/core-reserve/issues).
If you want to work on one, comment with your proposed change and how you
plan to verify it.

## Set up and run checks

Use x86-64 Linux with cgroup v2 and Rustup. The repository pins the Rust toolchain.

```sh
cargo build --release --locked
./target/release/corereserve doctor
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
cargo test --workspace --locked
```

Follow the [setup guide](docs/getting-started.md) for a smoke test.
Use the [delegated systemd lab](docs/host-lab.md) on a disposable host or VM
when testing quota writes.

## Report results

Include the commit or release, kernel version, CPU model, commands, and
observed behavior. For [benchmark results](docs/benchmark.md), include the
sanitized lab manifest, raw reports, p99.9, errors and timeouts, batch
throughput, and whether the load generator ran on a separate host.

## Submit a pull request

Link the relevant issue and describe the change, how you tested it, and
any kernel or cgroup assumptions. Contributions are made under the
[MIT License](LICENSE).
