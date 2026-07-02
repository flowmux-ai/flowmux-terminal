# Changelog

All notable changes to the `thorvg-sys` crate are documented here. The
format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Versioning.** This crate uses its **own** SemVer, decoupled from the
vendored ThorVG C++ release. The bundled ThorVG version is recorded as
SemVer **build metadata** — e.g. `0.1.0+thorvg-1.0.5` is crate `0.1.0`
bundling ThorVG `1.0.5`. Because the crate is `0.x`, a **minor** bump is
breaking; the safe [`thorvg`](../thorvg/CHANGELOG.md) crate's dependency
moves in lockstep.

## [0.2.1+thorvg-1.0.6] - 2026-06-15

Build-system and portability fixes; still bundles **ThorVG 1.0.6**. No
API change — a patch bump.

### Fixed

- **System (non-vendored) build now finds ThorVG 1.x.** `link_system`
  probed the unversioned `thorvg` pkg-config module, but ThorVG 1.x
  installs `thorvg-1` (meson `filebase: 'thorvg-' + vmaj`). Probe
  `thorvg-1` first, falling back to `thorvg` for distros that ship the
  unversioned name.
- **MSVC vendored build.** Define `NOMINMAX` — the Windows SDK's
  `min`/`max` macros mangled `tvgMath.h`'s `Point min(...)` / `max(...)`
  declarations (C2059) — and pass `/EHsc` for the standard C++ exception
  model.
- **Static linkage on MSVC.** Define `TVG_STATIC`: the vendored library
  is linked as a static archive, but without it the C API expanded to
  `__declspec(dllimport)` and MSVC rejected the definitions (C2491).

## [0.2.0+thorvg-1.0.6] - 2026-06-15

Bundles **ThorVG 1.0.6**. The minor bump reflects a breaking change to
the generated FFI surface (a removed C API symbol).

### Changed

- **Bumped vendored ThorVG to 1.0.6.** The seven bare-metal patches that
  sat on `v1.0.5` were rebased unchanged onto `v1.0.6`.

### Added

- `tvg_lottie_animation_set_audio_resolver`, the `Tvg_Audio_Resolver`
  callback type, and the `Tvg_Audio_Info` struct — new in ThorVG 1.0.6
  for synchronizing external audio playback with the Lottie timeline.
- `TVG_ENGINE_OPTION_ALIASED` — re-introduced upstream (disables
  anti-aliased rendering); it had been dropped in 1.0.5.

### Removed

- `tvg_lottie_animation_assign` — removed upstream in ThorVG 1.0.6.
  **Breaking** for any code that called it through the raw bindings.

## [0.1.0+thorvg-1.0.5] - 2026-06-13

First release under the crate's own versioning. **Supersedes the yanked
`1.0.0` / `1.0.1` / `1.0.5`**, which mirrored the upstream ThorVG version
number 1:1 — a scheme abandoned because it left no room to publish
sys-crate-only changes (build system, bare-metal support) while upstream
stayed at 1.0.5. Bundles **ThorVG 1.0.5**.

### Changed

- **Versioning scheme** — the crate version is now independent of
  upstream; the bundled ThorVG release is carried as `+thorvg-X.Y.Z`
  build metadata. The previous upstream-mirroring `1.0.x` releases are
  yanked.

### Build system

- **Replaced the meson + ninja build with the `cc` crate** — ThorVG is
  compiled from source via Cargo's configured (cross-)compiler; no meson
  or ninja required.
- **Feature-gated loaders and capabilities** — `lottie`, `svg`, `png`,
  `fonts`, `expressions`, `threads`, `file-io` Cargo features select
  which ThorVG components compile (all enabled by default; embedded users
  disable defaults and pick what they need).
- bindgen now passes an explicit `--target=` to libclang on host builds.

### Bare-metal support (`target_os = "none"`)

- Toolchain-agnostic cross-compilation pipeline, split into bare-metal
  vs. SDK-runtime policy.
- **Vendors picolibc** (submodule pinned to 1.8.11) as the libc on
  bare-metal: compile-time `picolibc.h` configuration plus a compile-only
  validation phase in `build.rs`.
- Per-concern runtime stubs welded to picolibc declarations with weak
  linkage; bridges newlib's `__errno()` to picolibc's plain `errno`;
  stubs `_on_exit`.
- RISC-V canonical-multilib selection; `expressions` enabled on
  bare-metal ESP32-C6.

### Vendored ThorVG patches

- Local shims layered on ThorVG 1.0.5 to support bare-metal builds:
  a `bsearch` shim, a Lottie-loader shim extension, and runtime-stub
  welding across `tvgLock.h`, `tvgInitializer.cpp`, `tvgRender.cpp`,
  `tvgSwRenderer.cpp`, and `tvgSwMemPool.cpp`.

[0.2.0+thorvg-1.0.6]: https://github.com/goyox86/thorvg-rs/releases/tag/thorvg-sys-v0.2.0
[0.1.0+thorvg-1.0.5]: https://github.com/goyox86/thorvg-rs/releases/tag/thorvg-sys-v0.1.0
