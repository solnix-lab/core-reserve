use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for (tool, key) in [
        ("RUSTC", "CORERESERVE_BUILD_RUSTC"),
        ("CARGO", "CORERESERVE_BUILD_CARGO"),
    ] {
        let executable = env::var_os(tool).unwrap_or_else(|| panic!("Cargo did not set {tool}"));
        let output = Command::new(executable)
            .arg("--version")
            .output()
            .unwrap_or_else(|error| panic!("Could not get {tool} version: {error}"));
        assert!(output.status.success(), "{tool} --version failed");
        let version = std::str::from_utf8(&output.stdout).expect("Tool version must be UTF-8");
        println!("cargo:rustc-env={key}={}", version.trim());
    }
}
