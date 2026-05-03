// RUSTC_WRAPPER shim for componentize-py-pie.
//
// Cargo invokes this binary in place of `rustc`, passing the real rustc path
// as the first argument followed by the rustc invocation that cargo would
// otherwise have run directly.  This shim:
//
//   1. Strips any `-C metadata=…` argument cargo injected (cargo's value is
//      derived from many varying inputs including rustc version, lockfile
//      state, etc., which makes `StableCrateId` non-reproducible).
//   2. Appends a deterministic `-C metadata=componentize-py-abi-v1::<name>::<version>`
//      derived from `--crate-name` (parsed from the cargo-supplied args) and
//      `CARGO_PKG_VERSION` (set by cargo per-invocation).
//   3. Forwards everything else to the real rustc.
//
// Combined with `RUSTC_FORCE_RUSTC_VERSION` (set in build.rs), this makes
// every Rust v0-mangled symbol's `Cs<HASH>_` disambiguator a pure function of
// `(crate_name, crate_version)` -- independent of rustc version, cargo
// lockfile, host triple, sysroot path, etc.
//
// See `build.rs::make_runtime` for where this shim is wired in.

use std::process::{Command, exit};

const ABI_TAG: &str = "componentize-py-abi-v1";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("rustc_shim: expected real rustc path as first arg");
        exit(2);
    }
    let rustc = args.remove(0);

    let crate_name = args
        .windows(2)
        .find(|w| w[0] == "--crate-name")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "unknown".into());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "x".into());

    let mut out: Vec<String> = Vec::with_capacity(args.len() + 2);
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        // Drop the two-arg form: `-C` followed by `metadata=...`
        if a == "-C" && args.get(i + 1).is_some_and(|v| v.starts_with("metadata=")) {
            i += 2;
            continue;
        }
        // Drop the single-arg form: `-Cmetadata=...`
        if a.starts_with("-Cmetadata=") {
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out.push("-C".into());
    out.push(format!("metadata={ABI_TAG}::{crate_name}::{version}"));

    let status = Command::new(&rustc)
        .args(&out)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("rustc_shim: failed to spawn {rustc}: {e}");
            exit(2);
        });
    exit(status.code().unwrap_or(1));
}
