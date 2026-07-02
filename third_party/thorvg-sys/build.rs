use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let thorvg_src = manifest_dir.join("thorvg");

    if cfg!(feature = "vendored") {
        build_vendored_cc(&thorvg_src, &manifest_dir, &out_dir);
    } else {
        link_system();
    }

    generate_bindings(&thorvg_src, &out_dir);
}

// ---------------------------------------------------------------------------
// cc-based vendored build
// ---------------------------------------------------------------------------

/// Build thorvg from vendored source via cc-rs.
///
/// Cross-compiler selection follows cc-rs's standard `CC_<triple>` /
/// `CXX_<triple>` contract; no vendor binary names are hardcoded.
fn build_vendored_cc(thorvg_src: &Path, manifest_dir: &Path, out_dir: &Path) {
    let src = thorvg_src.join("src");
    write_config_h(out_dir);

    let target = TargetInfo::from_env();
    let multilib = cross_toolchain_multilib_args(&target);

    let sources = collect_thorvg_sources(&src);
    let include_dirs = thorvg_include_dirs(thorvg_src, &src, out_dir);

    let mut build = configure_thorvg_build(&target, &multilib);

    let picolibc_root = manifest_dir.join("picolibc");
    let picolibc_config = manifest_dir.join("picolibc-config");
    if target.is_bare_metal {
        build_picolibc(&picolibc_root, &picolibc_config, &target.arch, &multilib).unwrap_or_else(
            |reason| {
                panic!(
                    "picolibc build failed for target_arch={}: {reason}\n\
                     \n\
                     thorvg-sys requires picolibc on `target_os == \"none\"`.\n\
                     To add a new arch, wire it into `picolibc_machine_subdir`\n\
                     and ensure `picolibc/libc/machine/<dir>/` exists.",
                    target.arch,
                )
            },
        );
        apply_picolibc_header_isolation(&mut build, &picolibc_root, &picolibc_config, &target.arch);
    }

    for dir in &include_dirs {
        build.include(dir);
    }
    for src_file in &sources {
        build.file(src_file);
    }
    build.compile("thorvg");

    println!("cargo:rustc-link-lib=static=thorvg");
    emit_runtime_link_directives(&target, &multilib);
    println!("cargo:rerun-if-changed=thorvg/src");
}

// ---------------------------------------------------------------------------
// Target classification
// ---------------------------------------------------------------------------

/// Cargo-derived target classification.
///
/// Three predicates drive every per-target decision:
///
///   * `is_bare_metal` (`target_os == "none"`) — no system libc / C++
///     runtime; triggers the picolibc compile and header isolation.
///   * `is_hosted` (gnu/musl/msvc env, or apple vendor) — system libc +
///     libstdc++ are present; cc-rs defaults are correct.
///   * neither — a self-contained SDK runtime owns the link surface;
///     we suppress directives that would conflict with it.
///
/// `is_msvc` is a separate flag-dialect branch in `configure_thorvg_build`.
struct TargetInfo {
    os: String,
    env: String,
    vendor: String,
    arch: String,
    is_bare_metal: bool,
    is_msvc: bool,
    is_hosted: bool,
}

impl TargetInfo {
    fn from_env() -> Self {
        let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let env_ = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        let vendor = env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let is_bare_metal = os == "none";
        let is_msvc = env_ == "msvc";
        let is_hosted = matches!(env_.as_str(), "gnu" | "musl" | "msvc") || vendor == "apple";
        Self {
            os,
            env: env_,
            vendor,
            arch,
            is_bare_metal,
            is_msvc,
            is_hosted,
        }
    }
}

// ---------------------------------------------------------------------------
// Source / include enumeration
// ---------------------------------------------------------------------------

/// Collect thorvg `.cpp` sources for the enabled cargo features.
///
/// Always compiled: renderer, CPU engine, common, C API binding, raw
/// loader.  Loaders (lottie / svg / png / fonts) are feature-gated;
/// JerryScript is pulled recursively when `expressions` is on.  GPU
/// engine sources are always excluded — thorvg-sys is SW-only.
fn collect_thorvg_sources(src: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();

    add_dir_cpp(&mut sources, &src.join("renderer"));
    add_dir_cpp(&mut sources, &src.join("renderer/cpu_engine"));
    add_dir_cpp(&mut sources, &src.join("common"));
    add_dir_cpp(&mut sources, &src.join("bindings/capi"));
    add_dir_cpp(&mut sources, &src.join("loaders/raw"));

    if cfg!(feature = "lottie") {
        add_dir_cpp(&mut sources, &src.join("loaders/lottie"));
        if cfg!(feature = "expressions") {
            add_dir_cpp_recursive(&mut sources, &src.join("loaders/lottie/jerryscript"));
        } else {
            // Drop the expressions TU — references JerryScript headers
            // we don't put on the include path.
            sources.retain(|p| !p.to_string_lossy().contains("tvgLottieExpressions"));
        }
    }
    if cfg!(feature = "svg") {
        add_dir_cpp(&mut sources, &src.join("loaders/svg"));
    }
    if cfg!(feature = "png") {
        add_dir_cpp(&mut sources, &src.join("loaders/png"));
    }
    if cfg!(feature = "jpg") {
        add_dir_cpp(&mut sources, &src.join("loaders/jpg"));
    }
    if cfg!(feature = "webp") {
        // The webp loader vendors a trimmed libwebp under dec/ dsp/
        // utils/, so its sources live in subdirectories.
        add_dir_cpp_recursive(&mut sources, &src.join("loaders/webp"));
    }
    if cfg!(feature = "fonts") {
        add_dir_cpp(&mut sources, &src.join("loaders/sfnt"));
    }

    sources.retain(|p| {
        let s = p.to_string_lossy();
        !s.contains("gpu_engine") && !s.contains("tvgGl") && !s.contains("tvgWg")
    });
    sources
}

/// Include-dir set matching `collect_thorvg_sources`.
///
/// `out_dir` carries the generated `config.h`; the rest mirror source
/// directories so cross-TU `#include "tvg*.h"` lookups resolve.
fn thorvg_include_dirs(thorvg_src: &Path, src: &Path, out_dir: &Path) -> Vec<PathBuf> {
    let mut include_dirs: Vec<PathBuf> = vec![
        out_dir.to_path_buf(),  // config.h
        thorvg_src.join("inc"), // thorvg.h public C++ header
        src.join("renderer"),
        src.join("renderer/cpu_engine"),
        src.join("common"),
        src.join("bindings/capi"),
        src.join("loaders/raw"),
    ];

    if cfg!(feature = "lottie") {
        include_dirs.push(src.join("loaders/lottie"));
        include_dirs.push(src.join("loaders/lottie/rapidjson"));
        if cfg!(feature = "expressions") {
            let jerry = src.join("loaders/lottie/jerryscript");
            for sub in &[
                "jerry-core",
                "jerry-core/include",
                "jerry-core/ecma/base",
                "jerry-core/ecma/builtin-objects",
                "jerry-core/ecma/builtin-objects/typedarray",
                "jerry-core/ecma/operations",
                "jerry-core/jcontext",
                "jerry-core/jmem",
                "jerry-core/jrt",
                "jerry-core/lit",
                "jerry-core/parser/js",
                "jerry-core/parser/regexp",
                "jerry-core/vm",
                "jerry-port/common",
            ] {
                include_dirs.push(jerry.join(sub));
            }
        }
    }
    if cfg!(feature = "svg") {
        include_dirs.push(src.join("loaders/svg"));
    }
    if cfg!(feature = "png") {
        include_dirs.push(src.join("loaders/png"));
    }
    if cfg!(feature = "jpg") {
        include_dirs.push(src.join("loaders/jpg"));
    }
    if cfg!(feature = "webp") {
        include_dirs.push(src.join("loaders/webp"));
    }
    if cfg!(feature = "fonts") {
        include_dirs.push(src.join("loaders/sfnt"));
    }
    include_dirs
}

// ---------------------------------------------------------------------------
// cc::Build configuration
// ---------------------------------------------------------------------------

/// Build a configured `cc::Build` for thorvg's C++ compile.
///
/// Include paths and source files are appended by the caller after
/// `apply_picolibc_header_isolation` has had a chance to inject
/// `-nostdinc` + picolibc's header tree.
fn configure_thorvg_build(target: &TargetInfo, multilib: &[String]) -> cc::Build {
    let mut build = cc::Build::new();
    build.cpp(true).std("c++14").warnings(false);

    // Expose POSIX surface (strdup, strncasecmp, …) on libcs that gate
    // it behind feature-test macros.
    build.define("_DEFAULT_SOURCE", None);

    // The vendored build is always linked as a static archive
    // (`cargo:rustc-link-lib=static=thorvg`), so the C API must carry no
    // Windows dllimport/dllexport decoration — otherwise MSVC rejects the
    // definitions as `dllimport` (C2491). Mirrors meson's `-DTVG_STATIC`.
    build.define("TVG_STATIC", None);

    // MSVC: the Windows SDK defines `min`/`max` as macros, which mangle
    // tvgMath.h's `Point min(...)` / `max(...)` declarations (C2059).
    // NOMINMAX suppresses them; /EHsc selects the standard C++ exception
    // model (exceptions are not disabled on MSVC, unlike the flags below).
    if target.is_msvc {
        build.define("NOMINMAX", None);
        build.flag("/EHsc");
    }

    // JerryScript's JERRY_VLA fallback uses alloca() without including
    // <alloca.h>.  Redirect to C99 VLAs.
    if cfg!(feature = "expressions") {
        build.define("JERRY_VLA(type,name,size)", "type name[size]");
    }

    // JerryScript's default global heap is 512 KB allocated as a single
    // `.bss` static array — too large for typical bare-metal SRAM
    // budgets.  16 KB fits most expression snippets used by Lottie.
    if cfg!(feature = "expressions") && target.is_bare_metal {
        build.define("JERRY_GLOBAL_HEAP_SIZE", "16");
    }

    // Mirror upstream's meson flag set (`src/meson.build`) — pure
    // code-shape choices, safe on every non-MSVC target.
    if !target.is_msvc {
        for f in &[
            "-fno-exceptions",
            "-fno-rtti",
            "-fno-stack-protector",
            "-fno-math-errno",
        ] {
            build.flag_if_supported(f);
        }
    }

    // Unwind stripping — bare-metal only.  SDK targets often assert on
    // EH-frame section layout in their linker scripts, so mismatched
    // unwind policy between our TUs and the SDK's would fail the link.
    if target.is_bare_metal {
        build.flag_if_supported("-fno-unwind-tables");
        build.flag_if_supported("-fno-asynchronous-unwind-tables");
    }

    // Suppress cc-rs's automatic `-lstdc++` / `-lc++` emission on any
    // non-hosted target.  Bare-metal has no system libstdc++; SDK
    // runtimes bring their own and would conflict with a duplicate.
    if !target.is_hosted {
        build.cpp_set_stdlib(None);
    }

    // Bare-metal C++ runtime plumbing:
    //   * `-fno-threadsafe-statics` suppresses `__cxa_guard_*` around
    //     function-local statics (would pull pthread stubs).
    //   * `-fno-use-cxa-atexit` avoids `__cxa_atexit` registrations
    //     (stubbed in `runtime_stubs.c`).
    //   * `-Os` because code size dominates bare-metal use.
    if target.is_bare_metal {
        build.flag_if_supported("-fno-threadsafe-statics");
        build.flag_if_supported("-fno-use-cxa-atexit");
        build.opt_level_str("s");
    }

    // Empty on every arch where cc-rs and the cross toolchain agree on
    // flag naming — see `cross_toolchain_multilib_args`.
    for f in multilib {
        build.flag(f);
    }

    build
}

/// Cross-toolchain multilib flag normaliser.
///
/// cc-rs ↔ GCC flag-naming mismatches that surface during runtime-
/// archive probes (`cross_runtime_libs`, `cross_sysroot_include`).
/// Currently only RISC-V is affected: cc-rs emits `-march=rv32imac`
/// while GCC's multilib is `rv32imac_zicsr_zifencei`, so the probe
/// returns the FPU-enabled libgcc whose soft-float helpers trap on a
/// no-FPU chip.  `-fno-rtti` is here so the probe doesn't pick the
/// `*/rtti/` multilib (we build `-fno-rtti`).
///
/// ARM / aarch64 cross toolchains agree with cc-rs naming and need
/// nothing here.  Add a branch for any future arch that diverges —
/// keep `build_picolibc` itself arch-agnostic.
///
/// Empty `Vec` on non-RISC-V or non-bare-metal targets.
fn cross_toolchain_multilib_args(target: &TargetInfo) -> Vec<String> {
    if !(target.is_bare_metal && target.arch == "riscv32") {
        return Vec::new();
    }

    let features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let has = |c: char| {
        features
            .split(',')
            .any(|f| f.eq_ignore_ascii_case(&c.to_string()))
    };
    let mut isa = String::from("rv32i");
    for ext in ['m', 'a', 'f', 'd', 'c'] {
        if has(ext) {
            isa.push(ext);
        }
    }
    isa.push_str("_zicsr_zifencei");
    let abi = if has('d') {
        "ilp32d"
    } else if has('f') {
        "ilp32f"
    } else {
        "ilp32"
    };
    vec![
        format!("-march={isa}"),
        format!("-mabi={abi}"),
        "-fno-rtti".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Header isolation & link emission
// ---------------------------------------------------------------------------

/// Switch thorvg's C++ compile from the toolchain's libc headers to
/// picolibc's.
///
/// `-nostdinc` drops libc, compiler builtins, AND libstdc++ together.
/// We re-add what we need: picolibc-config (resolves `<picolibc.h>` and
/// our `<pthread.h>` stub), the per-arch machine dir, the internal
/// cross-directory dirs (`libc/stdio`, `libc/locale`), the public
/// header tree, then `-isystem` for the toolchain's builtins and
/// libstdc++.
fn apply_picolibc_header_isolation(
    build: &mut cc::Build,
    picolibc_root: &Path,
    picolibc_config: &Path,
    target_arch: &str,
) {
    build.flag("-nostdinc");

    build.include(picolibc_config);
    let machine_subdir = picolibc_machine_subdir(target_arch)
        .expect("build_picolibc would have panicked on an unsupported arch");
    build.include(picolibc_root.join("libc/machine").join(machine_subdir));
    build.include(picolibc_root.join("libc/stdio"));
    build.include(picolibc_root.join("libc/locale"));
    build.include(picolibc_root.join("libc/include"));

    if let Some(builtin_inc) = cross_compiler_builtin_includes() {
        build.flag(format!("-isystem{}", builtin_inc.display()));
    }
    for cxx_inc in cross_cxx_include_paths() {
        build.flag(format!("-isystem{}", cxx_inc.display()));
    }
}

/// Emit `cargo:rustc-link-{search,lib}=` directives for the C++ runtime.
///
///   * Bare-metal: probe the cross toolchain for `libstdc++.a` /
///     `libc++.a` / `libsupc++.a` / `libgcc.a` / `libm.a` and emit
///     static link directives.  `libc.a` is intentionally not probed
///     (picolibc provides it, and pulling newlib's libc.a drags
///     objects that collide with HAL-defined symbols).  libm is
///     required — thorvg uses sqrt/sin/cos/atan2 extensively.
///   * Hosted: dynamic `c++` / `stdc++` request named per platform
///     (cc-rs auto-emit is unreliable on cross builds).
///   * Other: nothing — the SDK is responsible for the runtime, and
///     cc-rs auto-emit was already suppressed in `configure_thorvg_build`.
///
/// Note: `cargo:rustc-link-arg` from a sys crate applies only to that
/// crate's own link products (the rlib has no link step), so linker-
/// script-specific fixes must live in the consumer's `.cargo/config.toml`
/// or build.rs, not here.
fn emit_runtime_link_directives(target: &TargetInfo, multilib: &[String]) {
    if target.is_bare_metal {
        let found = cross_runtime_libs(
            &[
                "libstdc++.a",
                "libc++.a",
                "libsupc++.a",
                "libgcc.a",
                "libm.a",
            ],
            multilib,
        );
        let mut seen = std::collections::HashSet::new();
        for (dir, _) in &found {
            if seen.insert(dir.clone()) {
                println!("cargo:rustc-link-search=native={}", dir.display());
            }
        }
        for (_, name) in &found {
            println!("cargo:rustc-link-lib=static={name}");
        }
    } else if target.is_hosted {
        if target.vendor == "apple" || target.os == "freebsd" {
            println!("cargo:rustc-link-lib=dylib=c++");
        } else if target.os == "linux" || target.env == "gnu" {
            println!("cargo:rustc-link-lib=dylib=stdc++");
        }
    }
}

// ---------------------------------------------------------------------------
// Vendored picolibc (bare-metal libc)
// ---------------------------------------------------------------------------

/// Build picolibc as `libpicolibc.a` and emit link directives.
///
/// # Source enumeration
///
/// Walks a fixed set of `libc/<subtree>/` directories under the
/// vendored picolibc tree and applies two filters: a `denylist_files`
/// of file basenames, and basename suffix/prefix patterns (`*_l.c`,
/// `*_s.c`, `wcs*`, `mb*`, …) for whole categories we don't compile.
/// The arch-specific machine dir is walked separately with its own
/// `MACHINE_DENYLIST`.  Walking + denylist means a picolibc bump that
/// adds new files lands automatically; a one-time compile failure on
/// an unwanted new file is the signal to update the denylist.
///
/// # Architecture support
///
/// `picolibc_machine_subdir` is the single arch policy point: any
/// `target_arch` it maps to an existing machine dir is built; anything
/// else returns `Err` and the top-level caller panics.
///
/// ARM is deliberately not in the table — its machine dir ships
/// multiple ISA-variant `.S` files (per armv4t / armv6m / armv7m /
/// armv8m) that picolibc's meson selects between via `-mcpu=`.  A flat
/// walk would link-error on duplicate symbols.  Adding ARM means
/// porting that selection rule into this function.
fn build_picolibc(
    picolibc_root: &Path,
    picolibc_config: &Path,
    target_arch: &str,
    cross_toolchain_multilib_args: &[String],
) -> Result<(), String> {
    // ── Arch resolution ───────────────────────────────────────────────

    let machine_subdir = picolibc_machine_subdir(target_arch)
        .ok_or_else(|| format!("target_arch={target_arch} not mapped to a picolibc machine dir"))?;

    let machine_dir = picolibc_root.join("libc/machine").join(machine_subdir);
    if !machine_dir.is_dir() {
        return Err(format!(
            "picolibc machine dir missing: {}",
            machine_dir.display()
        ));
    }

    // ── Generic sources (walked + denylisted) ─────────────────────────

    let walk_dirs: &[&str] = &[
        "libc/ctype",
        "libc/string",
        "libc/stdlib",
        "libc/stdio",
        "libc/errno",
        "libc/search",
    ];

    let denylist_files = denylist_files();
    let denylist_suffixes: &[&str] = &["_l.c", "_s.c"];
    let denylist_prefixes: &[&str] = &[
        "wcs", "wmem", "wcp", "wcw", // wide-char
        "mblen", "mbr", "mbs", "mbt", "mbst", // multi-byte
    ];

    let mut sources: Vec<PathBuf> = Vec::new();
    for sub in walk_dirs {
        let dir = picolibc_root.join(sub);
        if !dir.is_dir() {
            return Err(format!("picolibc dir missing: {}", dir.display()));
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let Some(ext) = path.extension() else {
                continue;
            };
            if ext != "c" {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if denylist_files.contains(&name) {
                continue;
            }
            if denylist_suffixes.iter().any(|s| name.ends_with(s)) {
                continue;
            }
            if denylist_prefixes.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            sources.push(path);
        }
    }

    // ── Machine sources ───────────────────────────────────────────────
    //
    // Non-recursive walk for `.c` + `.S`; the nested `machine/` subdir
    // holds headers only.  `MACHINE_DENYLIST` covers TLS files
    // (deselected by `__SINGLE_THREAD` in picolibc.h).
    for entry in std::fs::read_dir(&machine_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext != "c" && ext != "S" {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if MACHINE_DENYLIST.contains(&name) {
            continue;
        }
        sources.push(path);
    }

    // ── thorvg-sys runtime stubs ──────────────────────────────────────
    //
    // Weak-symbol stubs for the pthread / getenv / getentropy /
    // _on_exit / __errno / _exit surface libsupc++ and newlib-built
    // archives pull in but picolibc doesn't ship.  Split per concern
    // (one `.c` per override unit) so a consumer's strong override
    // of, say, `_exit` makes the linker skip `hal.c` from the archive
    // entirely without dragging unrelated stubs.  Compiled into the
    // picolibc archive to share its include path and multilib config.
    let runtime_stubs_dir = picolibc_config.join("runtime_stubs");
    if !runtime_stubs_dir.is_dir() {
        return Err(format!(
            "picolibc-config runtime_stubs/ dir missing: {}",
            runtime_stubs_dir.display()
        ));
    }
    for entry in std::fs::read_dir(&runtime_stubs_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "c") {
            sources.push(path);
        }
    }

    // ── Include paths ─────────────────────────────────────────────────
    //
    // `picolibc-config/` must come first so `<picolibc.h>` resolves
    // to our authored config (upstream ships only a `.in` template).
    // Then the per-arch machine dir, then the cross-directory dirs
    // picolibc's internal bare-name includes rely on, then the public
    // header tree.  `libc/stdlib/` is on the path so `runtime_stubs.c`
    // can pull `local-onexit.h` for the `_on_exit` types.
    let include_dirs: Vec<PathBuf> = vec![
        picolibc_config.to_path_buf(),
        machine_dir.clone(),
        picolibc_root.join("libc/stdio"),
        picolibc_root.join("libc/locale"),
        picolibc_root.join("libc/stdlib"),
        picolibc_root.join("libc/include"),
    ];

    // ── cc::Build for picolibc ────────────────────────────────────────

    let mut build = cc::Build::new();
    build.warnings(false);
    build.cpp(false);

    // `-nostdinc` strips ALL system include paths; restore the
    // compiler's builtin-header dir via `-isystem` so picolibc's
    // `#include <stdio.h>` resolves to picolibc's, never newlib's.
    build.flag("-nostdinc");
    if let Some(builtin_inc) = cross_compiler_builtin_includes() {
        build.flag(format!("-isystem{}", builtin_inc.display()));
    }

    // Match thorvg's size-tuned flag set for consistent unwind / RTTI /
    // stack-protector policy across objects.
    for f in &[
        "-fno-stack-protector",
        "-fno-math-errno",
        "-fno-unwind-tables",
        "-fno-asynchronous-unwind-tables",
    ] {
        build.flag_if_supported(f);
    }
    build.opt_level_str("s");

    // Mirror outer cc-rs auto-suppression (we don't want libstdc++
    // linked twice — `emit_runtime_link_directives` handles it).
    build.cpp_set_stdlib(None);

    // Force-include `picolibc.h` so `.S` TUs routed through the C
    // preprocessor also pick up our config.
    let picolibc_h = picolibc_config.join("picolibc.h");
    build.flag(format!("-include{}", picolibc_h.display()));

    // Match thorvg's multilib so picolibc TUs build for the same ABI.
    for f in cross_toolchain_multilib_args {
        build.flag(f);
    }

    for dir in &include_dirs {
        build.include(dir);
    }
    for src in &sources {
        build.file(src);
    }

    // `try_compile` so a compile failure surfaces as `Err` (caller
    // wraps and panics) rather than cc-rs's own raw panic.
    build
        .try_compile("picolibc")
        .map_err(|e| format!("compile failed: {e}"))?;
    Ok(())
}

/// File-basenames excluded from the picolibc compile.
///
/// Filtered against `Path::file_name()` so subdirectory placement
/// doesn't matter — a `malloc.c` anywhere in the walked tree is dropped.
fn denylist_files() -> &'static [&'static str] {
    &[
        // Allocator — consumer provides malloc/free/realloc/calloc.
        "malloc.c",
        "free.c",
        "realloc.c",
        "calloc.c",
        "aligned_alloc.c",
        "memalign.c",
        "posix-memalign.c",
        "valloc.c",
        "pvalloc.c",
        "reallocarray.c",
        "reallocf.c",
        "mallinfo.c",
        "mallopt.c",
        "malloc-stats.c",
        "malloc-usable-size.c",
        // Environment / POSIX — no environment on bare metal.
        "getenv.c",
        "getenv_r.c",
        "putenv.c",
        "setenv.c",
        "environ.c",
        "system.c",
        "getopt.c",
        "getsubopt.c",
        "getauxval.c",
        "getpagesize.c",
        "rpmatch.c",
        // 48-bit PRNG family — duplicate generator with different
        // output types; keep `rand.c` / `srand.c` only.
        "drand48.c",
        "erand48.c",
        "jrand48.c",
        "lrand48.c",
        "mrand48.c",
        "nrand48.c",
        "seed48.c",
        "srand48.c",
        "lcong48.c",
        "rand48.c",
        // Re-entrant rand variant — single-threaded build.
        "rand_r.c",
        // BSD base-64 ASCII encoders.
        "a64l.c",
        "l64a.c",
        // `random.c` / `srandom.c` are KEPT: distinct POSIX API from
        // `rand()` and libstdc++ takes a weak external ref that
        // surfaces at link time if absent.
        //
        // `arc4random.c` is dropped — it pulls a chacha-based re-seed
        // loop that wants entropy.  `runtime_stubs.c` provides a
        // stub for libstdc++'s benefit.
        "arc4random.c",
        "arc4random_uniform.c",
        // C11 Annex K bounds-checking.  Most are caught by the
        // `_s.c` suffix denylist; these don't fit the pattern.
        "set_constraint_handler_s.c",
        "ignore_handler_s.c",
        "strerrorlen_s.c",
        // C++ ABI atexit glue — we build with `-fno-use-cxa-atexit`.
        "cxa-atexit.c",
        "onexit.c",
        "exitprocs.c",
        // Multi-byte / wide-char single files not caught by prefix.
        "btowc.c",
        "wctob.c",
        "wctomb.c",
        "wctomb_r.c",
        "sb_charsets.c",
        "ejtouc.c",
        "jitouc.c",
        "sjtouc.c",
        "uctoej.c",
        "uctoji.c",
        "uctosj.c",
        // Wide-char ctype.
        "ctype_wide.c",
        // Assert family pulling stdio paths (verbose `__assert_func`
        // from `assert_func.c` stays).
        "assert.c",
        "assert_no_arg.c",
        "eprintf.c",
        // POSIX wide-char console.
        "posixiob_stdin.c",
        "posixiob_stdout.c",
        "posixiob_stderr.c",
        // Stdio Ryu fast-but-large dtoa — `__IO_FLOAT_EXACT` unset in
        // picolibc.h selects the smaller engine-based dtoa.
        "atod_ryu.c",
        "atof_ryu.c",
        "dtoa_ryu.c",
        "ftoa_ryu.c",
        "ryu_divpow2.c",
        "ryu_log10.c",
        "ryu_log2pow5.c",
        "ryu_pow5bits.c",
        "ryu_table.c",
        "ryu_umul128.c",
        // Stdio templates — `#include`d from variant wrappers
        // (`vfprintf.c` etc.); not compiled standalone.  Upstream's
        // meson achieves this by not listing them; our walk needs
        // them named.
        "conv_flt.c",
        "ultoa_invert.c",
        "vfprintf_char.c",
        "vfprintf_float.c",
        "vfprintf_int.c",
        "vfprintf_n.c",
        "vfprintf_str.c",
        // Tree/hash search — we use bsearch + qsort only.
        "hash.c",
        "hash_bigkey.c",
        "hash_buf.c",
        "hash_func.c",
        "hash_log2.c",
        "hash_page.c",
        "hcreate.c",
        "hcreate_r.c",
        "ndbm.c",
        "tdelete.c",
        "tdestroy.c",
        "tfind.c",
        "tsearch.c",
        "twalk.c",
        "bsd_qsort_r.c",
        "qsort_r.c",
    ]
}

/// Files excluded from any `libc/machine/<arch>/` walk.  TLS setup;
/// `__SINGLE_THREAD` in `picolibc.h` deselects TLS.
const MACHINE_DENYLIST: &[&str] = &["tls.c", "inittls.c"];

/// Map a Rust `CARGO_CFG_TARGET_ARCH` value to picolibc's
/// `libc/machine/<dir>/` subdirectory name.
///
/// Single arch-policy point for the picolibc build.  Returns `None`
/// for arches picolibc doesn't ship and for arches that need per-ISA
/// variant selection a flat directory walk would mis-resolve (see
/// `build_picolibc`'s ARM caveat).
fn picolibc_machine_subdir(target_arch: &str) -> Option<&'static str> {
    match target_arch {
        "riscv32" | "riscv64" => Some("riscv"),
        "aarch64" => Some("aarch64"),
        "x86" => Some("i386"),
        "x86_64" => Some("x86_64"),
        "powerpc" | "powerpc64" => Some("powerpc"),
        "mips" | "mips64" => Some("mips"),
        "sparc" | "sparc64" => Some("sparc"),
        "m68k" => Some("m68k"),
        "msp430" => Some("msp430"),
        // arm intentionally absent — see build_picolibc.
        _ => None,
    }
}

/// Discover the cross-compiler's builtin-headers include directory.
///
/// Picolibc TUs `#include <stdarg.h>` / `<stddef.h>` / etc., which
/// are compiler-builtin (not libc).  `-nostdinc` strips them along
/// with libc; this probe restores only the builtins.
///
/// Returns `None` when the driver yields nothing useful; the compile
/// then fails loudly at the first builtin include, which is the
/// right signal.
fn cross_compiler_builtin_includes() -> Option<PathBuf> {
    let tool = cc::Build::new().try_get_compiler().ok()?;

    // GCC: `-print-file-name=include` returns the bundled include dir.
    let mut cmd = std::process::Command::new(tool.path());
    cmd.args(tool.args());
    cmd.arg("-print-file-name=include");
    if let Ok(res) = cmd.output() {
        let s = String::from_utf8_lossy(&res.stdout).trim().to_string();
        if !s.is_empty() && s != "include" {
            let p = PathBuf::from(s);
            if p.is_dir() {
                return Some(p);
            }
        }
    }

    // Clang: `-print-resource-dir` returns `<resource>`; the include
    // dir is `<resource>/include`.
    let mut cmd = std::process::Command::new(tool.path());
    cmd.args(tool.args());
    cmd.arg("-print-resource-dir");
    if let Ok(res) = cmd.output() {
        let s = String::from_utf8_lossy(&res.stdout).trim().to_string();
        if !s.is_empty() {
            let p = PathBuf::from(s).join("include");
            if p.is_dir() {
                return Some(p);
            }
        }
    }

    None
}

/// Discover the cross-compiler's C++ stdlib include search paths.
///
/// `-nostdinc` strips libstdc++ along with libc and builtins; this
/// probe restores libstdc++ for thorvg's C++ TUs.  Asks the driver
/// for its default include search list via `-E -x c++ -v`, parses
/// the diagnostic, keeps paths containing `/c++/` (universal marker
/// for libstdc++ / libc++ trees).
fn cross_cxx_include_paths() -> Vec<PathBuf> {
    let Ok(tool) = cc::Build::new().cpp(true).try_get_compiler() else {
        return Vec::new();
    };
    let mut cmd = std::process::Command::new(tool.path());
    cmd.args(tool.args());
    // Preprocess-only, C++ mode, verbose, stdin source — produces an
    // empty TU whose only purpose is to make the driver emit its
    // standard search-path diagnostic on stderr.
    cmd.arg("-E")
        .arg("-x")
        .arg("c++")
        .arg("-v")
        .arg("-")
        .stdin(std::process::Stdio::null());
    let Ok(res) = cmd.output() else {
        return Vec::new();
    };
    let stderr = String::from_utf8_lossy(&res.stderr);

    let mut paths = Vec::new();
    let mut in_block = false;
    for line in stderr.lines() {
        if line.contains("#include <...> search starts here:") {
            in_block = true;
            continue;
        }
        if line.contains("End of search list.") {
            break;
        }
        if !in_block {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Universal libstdc++ / libc++ marker.  libc paths from the
        // same prefix never contain `/c++/`.
        if !trimmed.contains("/c++/") {
            continue;
        }
        let p = PathBuf::from(trimmed);
        if p.is_dir() {
            paths.push(p);
        }
    }
    paths
}

// ---------------------------------------------------------------------------
// Cross-toolchain runtime-archive discovery
// ---------------------------------------------------------------------------

/// Discover bare-metal runtime archives from the configured cross-compiler.
///
/// For each `libfoo.a` in `wanted`, asks the cross C++ compiler for
/// its multilib-correct location via `-print-file-name=libfoo.a`.
/// Returns `(directory, link_name)` pairs in probe order.  Archives
/// the driver can't find are silently skipped (some toolchains fold
/// libsupc++ into libstdc++, for example).
fn cross_runtime_libs(wanted: &[&str], extra_args: &[String]) -> Vec<(PathBuf, String)> {
    let Ok(tool) = cc::Build::new().cpp(true).try_get_compiler() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(wanted.len());
    for file in wanted {
        let mut cmd = std::process::Command::new(tool.path());
        // Forward cc-rs's compile args + caller-supplied multilib
        // selectors so the probe returns the same multilib the cc
        // compile targets.  Without this, drivers tend to return the
        // default multilib (often FPU-enabled on RISC-V), producing
        // SIGILL on no-FPU chips.
        cmd.args(tool.args());
        for f in extra_args {
            cmd.arg(f);
        }
        cmd.arg(format!("-print-file-name={file}"));
        let Ok(res) = cmd.output() else { continue };
        let s = String::from_utf8_lossy(&res.stdout).trim().to_string();
        // Drivers echo the bare filename when they can't locate the lib.
        if s == *file {
            continue;
        }
        let p = PathBuf::from(s);
        if !p.is_file() {
            continue;
        }
        let Some(dir) = p.parent() else { continue };
        let link = file
            .trim_start_matches("lib")
            .trim_end_matches(".a")
            .to_string();
        out.push((dir.to_path_buf(), link));
    }
    out
}

/// Write `config.h` based on enabled cargo features.
fn write_config_h(out_dir: &Path) {
    let mut config = String::from(
        "/* Generated by thorvg-sys build.rs — do not edit. */\n\
         #pragma once\n\n\
         #define THORVG_CAPI_BINDING_SUPPORT 1\n\
         #define THORVG_CPU_ENGINE_SUPPORT 1\n\
         #define THORVG_VERSION_STRING \"1.0.6\"\n",
    );

    if cfg!(feature = "lottie") {
        config.push_str("#define THORVG_LOTTIE_LOADER_SUPPORT 1\n");
    }
    if cfg!(feature = "expressions") {
        config.push_str("#define THORVG_LOTTIE_EXPRESSIONS_SUPPORT 1\n");
    }
    if cfg!(feature = "svg") {
        config.push_str("#define THORVG_SVG_LOADER_SUPPORT 1\n");
    }
    if cfg!(feature = "png") {
        config.push_str("#define THORVG_PNG_LOADER_SUPPORT 1\n");
    }
    if cfg!(feature = "jpg") {
        config.push_str("#define THORVG_JPG_LOADER_SUPPORT 1\n");
    }
    if cfg!(feature = "webp") {
        config.push_str("#define THORVG_WEBP_LOADER_SUPPORT 1\n");
    }
    if cfg!(feature = "fonts") {
        config.push_str("#define THORVG_SFNT_LOADER_SUPPORT 1\n");
        config.push_str("#define THORVG_OTF_LOADER_SUPPORT 1\n");
        config.push_str("#define THORVG_TTF_LOADER_SUPPORT 1\n");
    }
    if cfg!(feature = "threads") {
        config.push_str("#define THORVG_THREAD_SUPPORT 1\n");
    }
    if cfg!(feature = "file-io") {
        config.push_str("#define THORVG_FILE_IO_SUPPORT 1\n");
    }

    std::fs::write(out_dir.join("config.h"), config).expect("failed to write config.h");
}

// ---------------------------------------------------------------------------
// System (non-vendored) build
// ---------------------------------------------------------------------------

/// Link against a system-installed thorvg via pkg-config.
///
/// ThorVG 1.x installs its pkg-config module as `thorvg-1` (meson
/// `filebase: 'thorvg-' + vmaj`).  Some distributions also ship an
/// unversioned `thorvg`, so try the versioned name first and fall back.
fn link_system() {
    let probe = |name| {
        pkg_config::Config::new()
            .atleast_version("1.0.0")
            .probe(name)
    };
    if probe("thorvg-1").is_err() && probe("thorvg").is_err() {
        panic!(
            "Could not find system thorvg >= 1.0.0 via pkg-config \
             (tried `thorvg-1` and `thorvg`). Either install thorvg or \
             enable the `vendored` feature."
        );
    }
}

// ---------------------------------------------------------------------------
// Bindgen
// ---------------------------------------------------------------------------

/// Generate Rust bindings from the C API header.
fn generate_bindings(thorvg_src: &Path, out_dir: &Path) {
    let capi_header = thorvg_src
        .join("src")
        .join("bindings")
        .join("capi")
        .join("thorvg_capi.h");

    println!("cargo:rerun-if-changed={}", capi_header.display());

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let is_cross = !target.is_empty() && target != host;

    let mut builder = bindgen::Builder::default()
        .header(capi_header.to_string_lossy())
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .clang_arg(format!("-I{}", out_dir.display()))
        .allowlist_function("tvg_.*")
        .allowlist_type("Tvg_.*")
        .allowlist_var("TVG_.*")
        .rustified_enum("Tvg_Result")
        .rustified_enum("Tvg_Colorspace")
        // `Tvg_Engine_Option` is a power-of-two bitflags enum
        // (see thorvg_capi.h: NONE = 0, DEFAULT = 1 << 0,
        // SMART_RENDER = 1 << 1).  `bitfield_enum` emits a
        // newtype with `BitOr` / `BitAnd` / `Not` impls so
        // combined values like `DEFAULT | SMART_RENDER` are
        // representable.  `rustified_enum` would force the
        // value to match a declared variant exactly, making OR
        // combinations unreachable from Rust.
        .bitfield_enum("Tvg_Engine_Option")
        .rustified_enum("Tvg_Mask_Method")
        .rustified_enum("Tvg_Blend_Method")
        .rustified_enum("Tvg_Type")
        .rustified_enum("Tvg_Stroke_Cap")
        .rustified_enum("Tvg_Stroke_Join")
        .rustified_enum("Tvg_Stroke_Fill")
        .rustified_enum("Tvg_Fill_Rule")
        .rustified_enum("Tvg_Text_Wrap")
        .rustified_enum("Tvg_Filter_Method")
        .use_core()
        .layout_tests(true);

    // bindgen invokes libclang directly rather than the cross- or
    // host compiler, so libclang's defaults — not Rust's — drive the
    // include search list and target ABI.  Two corrections:
    //
    //   * On cross builds, forward the cross sysroot's `include/`
    //     dir so headers like `<stdint.h>` resolve.
    //   * Always set `--target=`.  libclang's default target can
    //     disagree with Rust's `HOST` (e.g. a 32-bit ABI on a
    //     64-bit machine if only an i386 libclang is on the library
    //     path), which trips bindgen's debug-assert that
    //     `target_pointer_size() == size_of::<*mut ()>()`.
    if is_cross {
        if let Some(inc) = cross_sysroot_include() {
            builder = builder.clang_arg(format!("-I{}", inc.display()));
        }
        // libclang doesn't recognise vendor-specific OS fields in Rust
        // triples; strip to `<arch>-none-elf`, the LLVM triple it
        // understands across embedded targets.  Arch is the only field
        // that affects sizeof/alignof for `uint32_t` etc.
        builder = builder.clang_arg(format!("--target={target_arch}-none-elf"));
    } else {
        // Host build: pass Rust's full triple so libclang matches
        // the actual ABI.
        builder = builder.clang_arg(format!("--target={target}"));
    }

    let bindings = builder.generate().expect("Unable to generate bindings");

    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

/// Discover the cross-compiler's sysroot include directory via
/// `-print-sysroot` + `/include`.  Used to point libclang at the
/// cross toolchain's libc headers when generating bindings.
fn cross_sysroot_include() -> Option<PathBuf> {
    let tool = cc::Build::new().cpp(true).try_get_compiler().ok()?;
    let mut cmd = std::process::Command::new(tool.path());
    // Forward `-march` / `-mabi` etc. so toolchains with per-multilib
    // sysroots return the right include tree.
    cmd.args(tool.args());
    cmd.arg("-print-sysroot");
    let out = cmd.output().ok()?;
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sysroot.is_empty() {
        return None;
    }
    let inc = PathBuf::from(sysroot).join("include");
    inc.exists().then_some(inc)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect all `.cpp` files in a directory (non-recursive).
fn add_dir_cpp(out: &mut Vec<PathBuf>, dir: &Path) {
    if !dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "cpp") {
                out.push(path);
            }
        }
    }
}

/// Collect all `.cpp` files in a directory tree (recursive).
fn add_dir_cpp_recursive(out: &mut Vec<PathBuf>, dir: &Path) {
    if !dir.exists() {
        return;
    }
    for entry in walkdir(dir) {
        if entry.extension().is_some_and(|e| e == "cpp") {
            out.push(entry);
        }
    }
}

/// Simple recursive directory walk (avoids the `walkdir` crate dep).
fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                result.extend(walkdir(&path));
            } else {
                result.push(path);
            }
        }
    }
    result
}
