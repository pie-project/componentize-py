# Stable runtime symbols (`Cs<HASH>_` disambiguators)

This document describes a cross-version linking bug that affected
`componentize-py` builds in shared-modules (factored) mode, why it happened,
and how the fix wired into [`build.rs`](../build.rs) and
[`build-support/rustc_shim.rs`](../build-support/rustc_shim.rs) eliminates it.

---

## TL;DR

- **Symptom.** A Wasm component built by tool installation **A** could not be
  loaded against the shared modules emitted by tool installation **B** (or
  vice versa), even when both installations were nominally the same code.
  The host failed with:

  ```
  component imports core module `componentize-py-runtime`,
  but a matching implementation was not found in the linker
  ```

- **Root cause.** `componentize-py-runtime.wasm` exports ~2,000 Rust v0-mangled
  symbols (e.g. `_RNvNtNtCs<HASH>_4pyo38internal5state15register_decref`).
  The `Cs<HASH>_` token is the crate's `StableCrateId`, which `rustc` derives
  from the rustc version, the cargo-injected `-C metadata=…`, and other
  per-build inputs. Different installations therefore produced runtimes whose
  symbols spelt the same logical name with different `Cs<HASH>_` tokens, and
  app components carried *the names they observed at build time* into their
  import tables — tying each app to one specific runtime build.

- **Fix.** Three collaborating pieces:
  1. A tiny `RUSTC_WRAPPER` shim rewrites every `-C metadata=…` to a
     deterministic value derived from `(crate_name, crate_version)`.
  2. `RUSTC_FORCE_RUSTC_VERSION` pins the `cfg_version` contribution to
     `StableCrateId::new` for *normal* crates.
  3. A `runtime/rust-toolchain.toml` file pins the nightly toolchain
     itself, because rustc's allocator/panic-handler shim crate
     (`___rustc`) intentionally hashes the *real* rustc version into
     its mangled name and explicitly ignores `RUSTC_FORCE_RUSTC_VERSION`.

  Together these make every `Cs<HASH>_` in `componentize-py-runtime.wasm`
  a pure function of `(crate_name, crate_version, pinned_nightly)`, so two
  independent installations produce byte-identical runtime wasm
  (verified — see [§8](#8-how-to-verify)).

---

## 1. Background: factored componentization and dynamic linking

`componentize-py` packages a Python application as a single WebAssembly
component. In *factored* mode (`--shared-modules auto`), it splits common
machinery — the CPython interpreter, the `componentize-py` Rust runtime,
musl/libc, WASI emulation shims, NumPy native extensions, etc. — into
separate `.wasm` modules that live alongside the app component. The host
loads those modules once and links them into every app component it
instantiates, mirroring how a desktop process shares `libc.so` across
executables.

Linking happens via the **wasm32-wasip1 PIC dynamic-linking ABI**:

- Each shared module is compiled position-independent (`-C relocation-model=pic`).
- Cross-module function calls go through a per-app **`GOT.func`** import
  table; cross-module data accesses go through a **`GOT.mem`** table.
- A synthesized **`__init`** module wires everything together: at component
  instantiation time it populates the GOTs from the shared modules' exports
  and runs each module's `__wasm_apply_data_relocs` and `_initialize`.
- Every entry that `__init` writes is keyed by **the symbol name as it
  appeared in the shared module at the time the app was built.**

That last point is what made the bug possible.

---

## 2. The symptom

The bug surfaced in a 2x2 cross-installation matrix: build the same demo app
with two installations of the tool (`A` = a published wheel, `B` = a local
`cargo run --release`), and then try to run each app against each
installation's shared modules.

| app from \\ libs from | A | B |
| --- | --- | --- |
| **A** | PASS | FAIL |
| **B** | FAIL | PASS |

The failure was always the same: the synthesized `__init` module imported
*specific* symbol names from the `componentize-py-runtime` core module, and
those names were not present as exports of the runtime that the host
actually loaded.

Spot-checking the names with `wasm-tools print` showed them to be Rust v0
mangled, with the divergence concentrated in the `Cs<HASH>_` token:

```text
app A imports:   _RNvNtNtCs4PUeqHE95we_4pyo38internal5state15register_decref
runtime B exports: _RNvNtNtCsioJB8758LAq_4pyo38internal5state15register_decref
                              ^^^^^^^^^^^
                              different StableCrateId
```

Logically the same function in `pyo3::internal::state::register_decref`, but
with a `Cs<HASH>_` token derived from a *different* `StableCrateId`.

### 2.1 A second failure mode found in production

After the wrapper described in [§6.1–6.2](#6-the-fix) was deployed, a
second variant of the same failure was reported: an app component built on
**macOS** still failed to load against shared modules built on **Linux**,
even when both hosts ran the same `cargo +1.95.0 build` against the same
source tree. Comparing the two `componentize-py-runtime.wasm` files:

```text
Cs tag set diff (mac runtime vs linux runtime):
  Cs7tEtGGQCaN4_   (linux only)
  CscXdVywKbHIJ_   (mac only)
```

Out of 28 distinct `Cs<HASH>_` tokens in each runtime, **27 matched and
exactly one differed**. The differing token belonged to 18 symbols, all of
the form `_RNvCs<TAG>_7___rustc...` — the rustc-internal allocator and
panic-handler shim crate. Those 18 names are also baked into the app
component's *expected type* for the `componentize-py-runtime` core module,
and the component-model subtype check fails when the expected names are not
exports of the runtime the host actually loads.

This pointed at one specific gap in the wrapper-based fix, addressed by
the toolchain pin described in [§6.3](#63-runtimerust-toolchaintoml).

---

## 3. Why the runtime contains so many Rust-mangled symbols

`componentize-py-runtime` is a `cdylib` written in Rust that calls into many
Rust crates: `pyo3`, `serde_json`, `num_bigint`, `hashbrown`, `core`, `std`,
etc. Because it is compiled as **PIC** for the wasm32-wasip1 dynamic-linking
ABI, *every* cross-crate function call goes through the GOT — including
calls between transitive dependencies bundled into the same `.so`.

Concretely, the runtime built locally currently exposes:

- **2,188** Rust v0-mangled exports (`_R…` symbols)
- **124** Rust v0-mangled imports (`GOT.func` / `GOT.mem` entries the runtime
  re-resolves against itself at instantiation)

When `wit-component`'s `Linker` synthesizes the app's `__init` module, it
copies the *exported* names of every shared module verbatim into the app's
import list. So those 2,188 names become a hard ABI contract between the
specific app component and the specific runtime it was built against.

---

## 4. Where the `Cs<HASH>_` token comes from

The `Cs<HASH>_` segment of a Rust v0-mangled name is the `StableCrateId` of
the crate that defined the symbol. The implementation is in `rustc_span`:

```rust
// rustc_span/def_id.rs (paraphrased)
impl StableCrateId {
    pub fn new(
        crate_name: Symbol,
        is_exe: bool,
        mut metadata: Vec<String>,
        cfg_version: &'static str,
    ) -> StableCrateId {
        let mut hasher = StableHasher::new();
        crate_name.hash(&mut hasher);
        metadata.sort();
        metadata.hash(&mut hasher);
        is_exe.hash(&mut hasher);
        cfg_version.hash(&mut hasher);
        StableCrateId(hasher.finish())
    }
}
```

The four hashed inputs are:

1. **`crate_name`** — stable across builds.
2. **`metadata`** — comes from every `-C metadata=…` flag passed to `rustc`.
   Cargo *always* injects one such flag per invocation, derived from the
   resolver state, lockfile, profile, host triple, etc. Its value can change
   from build to build even when the source code is identical.
3. **`is_exe`** — fixed by the crate type.
4. **`cfg_version`** — the rustc compiler version string. Changes whenever
   the toolchain is updated.

So the `Cs<HASH>_` token bakes in the environment of whatever machine
compiled the crate. Two installations of `factored-componentize-py` built at
different times — different rustc nightly, different cargo lockfile snapshot,
different sysroot path — *will* end up with different `Cs<HASH>_` tokens for
every transitive crate.

---

## 5. Why simpler fixes did not work

A few approaches looked appealing but were rejected:

| Approach | Why it fails |
| --- | --- |
| **`-C metadata=…` via `RUSTFLAGS`** | `RUSTFLAGS` *appends* to cargo's flags, so cargo's varying metadata is still hashed in. There is no `--no-default-metadata` knob. |
| **`-C metadata=…` via `[build].rustflags`** | Same as above. |
| **`#[no_mangle]` on the public surface** | The runtime's *public* surface is small (`__set_app_data`, `__prepare_snapshot`, `cabi_realloc`, …). The 2,188 problem symbols are *transitive internals* from `pyo3`, `core`, etc., that the wasm32-wasip1 PIC ABI forces across the GOT. We would have to fork every dependency. |
| **Post-process the wasm to rewrite mangled names** | Possible, but invasive: must parse the wasm, rewrite name section + export/import sections + GOT relocations, all without breaking link semantics. Strictly more code than the wrapper. |
| **Use the wrapper alone (no nightly pin)** | Covers ~98% of the symbols but misses the 18 `___rustc` allocator/panic-shim exports. Those go through `rustc::mangle_internal_symbol`, which hashes `tcx.sess.cfg_version` directly and explicitly ignores `RUSTC_FORCE_RUSTC_VERSION` (see [`compiler/rustc_symbol_mangling/src/v0.rs`](https://github.com/rust-lang/rust/blob/master/compiler/rustc_symbol_mangling/src/v0.rs#L87) — the comment says: *"RUSTC_FORCE_RUSTC_VERSION is ignored here as otherwise different we would get an abi incompatibility with the standard library"*). The only knob is to actually pin the rustc version, hence [§6.3](#63-runtimerust-toolchaintoml). |

The wrapper plus the toolchain pin are the smallest knobs that target the
actual root causes — `StableCrateId` and `cfg_version` — directly.

---

## 6. The fix

Three collaborating pieces, all in this repo. **No change to `src/`.**

### 6.1 `build-support/rustc_shim.rs`

A standalone ~50 LOC binary, zero dependencies. Cargo's `RUSTC_WRAPPER`
contract puts the real `rustc` path in `argv[1]`, followed by the rustc
invocation cargo would otherwise have run. The shim:

1. Captures the real rustc path.
2. Reads `--crate-name <name>` from the args.
3. Reads `CARGO_PKG_VERSION` from the env (cargo sets this per invocation).
4. Strips any `-C metadata=…` (handles both the two-arg `"-C", "metadata=…"`
   and the single-arg `"-Cmetadata=…"` forms).
5. Appends `-C metadata=componentize-py-abi-v1::<crate_name>::<version>`.
6. Invokes the real rustc with the rewritten argv and propagates the exit code.

The `componentize-py-abi-v1` prefix is a versioned tag: if we ever need to
intentionally break ABI, bump it.

### 6.2 `build.rs::make_runtime`

Two env vars are added to the existing `cmd.env(...)` chain:

```rust
let shim = build_rustc_shim(out_dir)?;

cmd.env("RUSTFLAGS", "-C relocation-model=pic --cfg pyo3_disable_reference_pool")
   .env("CARGO_TARGET_DIR", out_dir.join(target))
   .env("PYO3_CONFIG_FILE", out_dir.join("pyo3-config.txt"))
   .env("RUSTC_WRAPPER", &shim)
   .env("RUSTC_FORCE_RUSTC_VERSION", "componentize-py-abi-v1");
```

`build_rustc_shim` is a one-shot helper that compiles
`build-support/rustc_shim.rs` into `OUT_DIR` with `rustc -O --edition=2021`
and returns its path. It also emits a `cargo:rerun-if-changed` for the source.

`RUSTC_FORCE_RUSTC_VERSION` is the rustc-side override for the `cfg_version`
input to `StableCrateId::new`. It has lived in the rustc source unconditionally
for years, but it *is* documented as a testing aid — the comment block in
`make_runtime` flags this and notes that the fallback if it ever disappears
is the post-process rewrite mentioned in the table above.

### 6.3 `runtime/rust-toolchain.toml`

The wrapper covers cargo-tracked crates, but rustc *itself* synthesises a
small `___rustc` shim crate during compilation of any binary or `cdylib` that
needs a global allocator and panic handler. That shim's symbols are mangled
by [`rustc::mangle_internal_symbol`](https://github.com/rust-lang/rust/blob/master/compiler/rustc_symbol_mangling/src/v0.rs#L87),
which deliberately hashes `tcx.sess.cfg_version` directly and ignores
`RUSTC_FORCE_RUSTC_VERSION`. Cargo never invokes `rustc --crate-name ___rustc`,
so the wrapper is never given the chance to rewrite `-C metadata` for it.

The only way to stabilise those 18 `___rustc`-prefixed `Cs<HASH>_` tokens is
to pin the actual rustc version that compiles the runtime crate.
`runtime/rust-toolchain.toml` does exactly that:

```toml
[toolchain]
channel = "nightly-2026-02-28"
components = ["rust-src"]
profile = "minimal"
```

The pinned date is the last 1.95.0-nightly snapshot — that codebase later
shipped as 1.95.0-beta and then as 1.95.0 stable, so the revision has had
several weeks of beta soak followed by a stable release before being adopted
here. `rust-src` is required for `-Z build-std=panic_abort,std`; `profile =
"minimal"` keeps `rustup` from pulling tools we do not need.

`build.rs::make_runtime` no longer wraps the cargo invocation with `rustup
run nightly`; it just invokes `cargo`, and the toolchain file in
`runtime/` causes cargo to delegate to the pinned nightly automatically
(installing it on first run via `rustup`).

### 6.4 What this guarantees

After the change, every input to `StableCrateId::new` is fixed for *both*
cargo-tracked crates and rustc-internal mangling:

| Input | Pinned to | Mechanism |
| --- | --- | --- |
| `crate_name` | The crate's name (already stable). | — |
| `metadata` | `componentize-py-abi-v1::<crate_name>::<crate_version>`. | wrapper rewrites `-C metadata=…` |
| `is_exe` | The crate's type (already stable). | — |
| `cfg_version` (regular crates) | The constant `componentize-py-abi-v1`. | `RUSTC_FORCE_RUSTC_VERSION` |
| `cfg_version` (`mangle_internal_symbol`) | The version string of the pinned nightly. | `runtime/rust-toolchain.toml` |

The resulting `Cs<HASH>_` token is therefore a pure function of
`(crate_name, crate_version)` for ordinary crates and of `(crate_name,
pinned_nightly_version)` for the `___rustc` shim. Both are content-addressed
identities. Two installations of the tool on two machines (regardless of
host OS or the user's *outer* `rustup default`) will emit byte-identical
`componentize-py-runtime.wasm`.

---

## 7. Risks and limitations

- **Diamond dependencies on the same crate at two versions.** Including
  `CARGO_PKG_VERSION` in the metadata means `serde 1.0.x` and `serde 1.1.y`
  still get distinct `StableCrateId`s, as required for symbol uniqueness.
- **Cargo's incremental cache key includes `-C metadata`.** Our value is
  deterministic per `(name, version)`, so cache keys are stable across
  rebuilds — slightly *better* cache reuse than before.
- **Build scripts and proc-macros are also rewritten.** Harmless: those
  compile for the host triple and never appear in the runtime wasm. The
  only effect is consistent metadata, which again helps cache hits.
- **`RUSTC_FORCE_RUSTC_VERSION` is documented as a testing aid.** The
  comment in `make_runtime` records the fallback (post-process rewrite of
  the wasm) if rustc ever removes the override.
- **Lockfile drift between releases.** If `Cargo.lock` resolves a transitive
  dep to a different version in two releases of the tool, the dep's
  `crate_version` differs and so does its `Cs<HASH>_`. Crates whose major
  versions stay pinned therefore stay ABI-stable; this is the same
  granularity as semver promises and feels right.
- **rustc compiler upgrades that change the v0 mangling scheme.** Out of
  scope of this fix; would also be a Rust-wide event.
- **Bumping the pinned nightly is an ABI-breaking change for the runtime.**
  Every previously-built application component embeds the old `___rustc`
  `Cs<HASH>_` in its expected-type for the runtime core module, and a runtime
  built under a different nightly will not satisfy that subtype check. The
  `runtime/rust-toolchain.toml` header documents this; bump it in lockstep
  with a `componentize-py-abi-v1` → `componentize-py-abi-v2` style transition
  in the wrapper's metadata tag, and rebuild every shipped app component.
- **Why nightly at all.** `-Z build-std=panic_abort,std` is needed to
  recompile `std` with `-C relocation-model=pic` (pre-built `std` from
  rustup is not PIC and therefore cannot be linked into a wasm32-wasip1
  PIC dynamic-linking shared module) and to swap unwinding panics for
  `panic_abort` (WASI preview1 has no exception mechanism). Both are
  unstable cargo flags; until `-Z build-std` stabilises upstream, nightly
  is the only option, and pinning a known-good nightly is the right
  containment.

---

## 8. How to verify

Two checks are useful, neither of which requires anything beyond this repo
plus a second rustup toolchain (e.g. `rustup toolchain install 1.94.0`).

### 8.1 Determinism check

Build the parent crate twice into two distinct target directories, using two
different ambient toolchains, then compare the runtime artifact:

```sh
# Build A
CARGO_TARGET_DIR=/tmp/cppie-A cargo +stable build --release

# Build B
CARGO_TARGET_DIR=/tmp/cppie-B cargo +1.94.0 build --release

# The runtime artifact lives at:
#   $CARGO_TARGET_DIR/release/build/factored-componentize-py-*/out/libcomponentize_py_runtime_sync.so
A=$(ls /tmp/cppie-A/release/build/factored-componentize-py-*/out/libcomponentize_py_runtime_sync.so)
B=$(ls /tmp/cppie-B/release/build/factored-componentize-py-*/out/libcomponentize_py_runtime_sync.so)

# Compare the sets of Rust-mangled symbols.
diff \
  <(wasm-tools print "$A" | grep -oE '_R[A-Za-z0-9_]+' | sort -u) \
  <(wasm-tools print "$B" | grep -oE '_R[A-Za-z0-9_]+' | sort -u)

# Optional, stronger: byte-for-byte equality.
cmp "$A" "$B" && echo "byte-identical"
```

After this fix the `diff` is empty, and on the toolchain combinations we
have exercised the runtime wasm is in fact byte-identical.

### 8.2 Manual sanity check on the disambiguators

The set of `Cs<HASH>_` tokens printed by

```sh
wasm-tools print path/to/componentize-py-runtime.wasm \
  | grep -oE 'Cs[A-Za-z0-9]+_' \
  | sort -u
```

should now be identical across cargo-cleaned rebuilds on different
toolchains. Each token corresponds to one Rust crate that ended up linked
into the runtime; they should also stay stable across releases of this tool
as long as the corresponding `(crate_name, crate_version)` pair does not
change in `Cargo.lock`.

### 8.3 End-to-end cross-installation check

Build the same Python app with two separate installations of the tool, copy
the resulting `*_shared.wasm` from one and the `shared/` + `runtime/`
directories from the other, and instantiate them together in any host that
links wasm components. Once both installations carry this fix, all four
combinations of `(app from A or B, libs from A or B)` succeed. Wheels
published before this fix will still fail in the cross-installation
combinations, by design — they were built with drifting disambiguators.

---

## 9. File index

| Path | Role |
| --- | --- |
| [`build-support/rustc_shim.rs`](../build-support/rustc_shim.rs) | The wrapper binary. Compiled at build time, invoked by cargo per `rustc` call. |
| [`build.rs`](../build.rs) (`build_rustc_shim`, `make_runtime`) | Compiles the shim, wires `RUSTC_WRAPPER` + `RUSTC_FORCE_RUSTC_VERSION` into the runtime build, and invokes `cargo` directly (no `rustup run nightly`) so the toolchain file in `runtime/` is honoured. |
| [`runtime/rust-toolchain.toml`](../runtime/rust-toolchain.toml) | Pins the nightly toolchain used to compile the runtime. Required because rustc's `mangle_internal_symbol` ignores `RUSTC_FORCE_RUSTC_VERSION`. |
| [`runtime/`](../runtime) | The `componentize-py-runtime` Rust crate whose mangled symbols this fix stabilises. |
| [`src/link.rs`](../src/link.rs) | The host-side linker that turns shared modules into app components and synthesises `__init`. |
