# rav1d, vendored for Unflash

This is **rav1d 1.1.0**, the Rust port of the dav1d AV1 decoder
(<https://github.com/memorysafety/rav1d>, BSD-2-Clause, see `COPYING`), as
published on crates.io: upstream commit
`782dab2135ea64a057c097088a13eb8ed3cc3320`, ported from dav1d `966d63c1`.
`crates/unflash-av1` wraps its dav1d-compatible API (`src/lib.rs`) to decode
AV1 in the browser (wasm32-unknown-unknown) and natively for the tests.

It is vendored rather than taken from crates.io because the published crate
does not build for wasm32-unknown-unknown, and because its build script needs
nasm and a C compiler for the assembly, which Unflash does not use.

`COPYING` is upstream's licence text, fetched from
<https://raw.githubusercontent.com/memorysafety/rav1d/main/COPYING>
(identical to the copy in the 1.1.0 crate).

## What was left out

Everything the pure-Rust build does not need:

- all the assembly: `src/x86/*.asm`, `src/ext/x86/x86inc.asm`,
  `src/arm/asm.S`, `src/arm/32/*.S`, `src/arm/64/*.S`;
- `build.rs`, which only assembled those files (with `nasm-rs` and `cc`)
  when the `asm` feature was on;
- `Cargo.lock`, `Cargo.toml.orig`, `.cargo_vcs_info.json`, `.github/`,
  `.gitlab-ci.yml`, `.gitignore`, `CONTRIBUTING.md`, `NEWS.dav1d`,
  `THANKS.md`, `gcovr.cfg`, `retranspile.sh`, `rust-toolchain.toml`.

Kept: `lib.rs`, `src/**/*.rs`, `include/**/*.rs`, `README.md` (upstream's),
`COPYING`.

## `Cargo.toml`

Rewritten from the published one:

- features: only `bitdepth_8` and `bitdepth_16` (both on by default); the
  `asm`, `asm_arm64_dotprod`, `asm_arm64_i8mm` and `asm_arm64_sve2` features
  are gone, and so are the build dependencies `cc` and `nasm-rs` and the
  `raw-cpuid` dependency (used only by the assembly's CPU detection). The
  sources still test `feature = "asm"` in places; those code paths are
  simply never compiled, and `[lints.rust] unexpected_cfgs` declares the old
  feature names so the compiler does not warn about them;
- `build = false`, `publish = false`, an `rlib` only (upstream also builds a
  `staticlib` for C users);
- the `[profile.*]` sections are gone: profiles belong to the workspace root
  (whose release profile is upstream's: fat LTO, one codegen unit, `panic =
  "abort"`; in dev builds it gives rav1d `opt-level = 2` and no debug
  assertions: unoptimised, and with `DisjointMut`'s debug-only checking of
  every borrow, the AV1 tests take many minutes);
- `[lib] test = false, doctest = false`: upstream's own unit tests and doc
  examples are not run with Unflash's (`cargo test --workspace`); Unflash
  tests the decoder through `crates/unflash-av1/tests`;
- `[lints.rust]` allows `dead_code` (helpers only the assembly used),
  `mismatched_lifetime_syntaxes` and
  `unpredictable_function_pointer_comparisons` (upstream code that newer
  compilers warn about): a path dependency's warnings are not capped like a
  registry crate's, so they would show in every build of the workspace.

## Source patches: wasm32-unknown-unknown

These are the only changes to the Rust sources. On wasm32-unknown-unknown the
`libc` crate has no `ptrdiff_t`, `intptr_t`, `uintptr_t` or `off_t`, and no
errno constants, so the crate does not compile there. Each `use libc::X;`
became

```rust
#[cfg(not(target_family = "wasm"))]
use libc::X;
#[cfg(target_family = "wasm")]
#[allow(non_camel_case_types)]
type X = isize; // (usize for uintptr_t, i64 for off_t)
```

in these files (the types are the ones `libc` gives on other 32- and 64-bit
targets, so nothing changes in size or meaning):

| file | aliases |
|---|---|
| `include/dav1d/common.rs` | `off_t = i64` (and the two `pub offset: libc::off_t` fields of `Dav1dDataProps` / `Rav1dDataProps` became `off_t`) |
| `include/dav1d/picture.rs` | `ptrdiff_t = isize`, `uintptr_t = usize` |
| `src/cdef.rs`, `src/cdef_apply.rs`, `src/decode.rs`, `src/internal.rs`, `src/ipred.rs`, `src/lf_mask.rs`, `src/loopfilter.rs`, `src/lr_apply.rs`, `src/picture.rs` | `ptrdiff_t = isize` |
| `src/filmgrain.rs` | `intptr_t = isize`, `ptrdiff_t = isize` |
| `src/looprestoration.rs` | `ptrdiff_t = isize`, and `intptr_t = isize` inside its inner (assembly-binding) module |
| `src/recon.rs` | `intptr_t = isize` |

and in `src/error.rs` each errno-valued variant of `Rav1dError` got its
Linux value under `cfg(target_family = "wasm")`:

```rust
#[cfg(not(target_family = "wasm"))]
EAGAIN = libc::EAGAIN as u8,
#[cfg(target_family = "wasm")]
EAGAIN = 11,
```

(`ENOENT` 2, `EIO` 5, `EAGAIN` 11, `ENOMEM` 12, `EINVAL` 22, `ERANGE` 34,
`ENOPROTOOPT` 92). The API returns these negated (`DAV1D_ERR`), so a caller
on wasm compares with those values: `unflash-av1` does, for `EAGAIN`.

With that, `cargo build --release --target wasm32-unknown-unknown` builds the
crate with `default-features = false, features = ["bitdepth_8"]` or with both
bit depths, and the decoder runs there on one thread (`n_threads = 1`: no
thread is started, and no lock is ever waited on).

## Notes

- `bitdepth_16` off does not leave out the 16-bit pixel code:
  `Rav1dBitDepthDSPContext::get` (`src/internal.rs`) instantiates it for 10-
  and 12-bit streams whatever the features, which only gate film grain and a
  few other paths. Measured in a wasm module that decodes with
  `unflash-av1` (release, `wasm-opt -O3`): 1,184 KB with both bit depths,
  1,176 KB with `bitdepth_8` alone, 1,049 KB when those two match arms are
  also left out, so 10-bit costs about 135 KB. Unflash decodes 10-bit.
- rav1d is memory-safe but may panic on some malformed streams (checked
  indexing, `unwrap`s); with `panic = "abort"` that aborts the process, or
  traps the wasm instance, which then has to be thrown away.
- To update: take the new release's sources, drop the same files, re-apply
  the patches above (`grep -rn "use libc::" src include`, and `src/error.rs`),
  carry the `Cargo.toml` over, and run `cargo test -p unflash-av1 --release`
  plus a wasm32 build.
