//! Runtime ONNX Runtime initialization.
//!
//! ONNX Runtime backs OCR and the semantic reranker. It is acquired,
//! linked and initialized differently on every target:
//!
//! - **Linux x86_64** : dlopen'd at runtime behind an AVX gate
//! - **Linux aarch64** : no ONNX; released without `ocr,rerank`
//! - **macOS arm64** : statically linked
//! - **macOS x86_64** : no ONNX; released without `ocr,rerank`
//! - **Windows x64** : statically linked; imports a `DirectML.dll` it
//!   never calls
//!
//! Which targets get OCR/rerank at all is decided in the release matrix
//! (`.github/workflows/release.yml`).
//!
//! **The one rule that has already broken a release:** declare `ort` only
//! in the two mutually exclusive `[target.'cfg(...)'.dependencies]`
//! sections of `Cargo.toml`, never in shared `[dependencies]`. Cargo
//! **unions** features across every target section whose cfg matches : it
//! does not pick one : so a shared entry leaks Linux's `load-dynamic` onto
//! Windows/macOS, where it wins over static linking and ships a binary
//! with no ONNX in it, no dylib beside it, and no error anywhere. That is
//! exactly how v3.3.0 went out with OCR and rerank dead on win32-x64 and
//! darwin-arm64.
//!
//! Everything below is reference detail: per-platform rationale, a
//! postmortem of the v3.3.0 wiring bug, and the Windows DirectML story.
//! Read the section for the platform or failure you are actually touching.
//!
//! ## Linux x86_64 : dlopen behind an AVX gate
//!
//! Dynamically loaded to avoid SIGILL on non-AVX CPUs. The prebuilt ONNX
//! static archive contains unguarded AVX instructions in C++ global
//! constructors that run before `main()`, so statically linking it kills
//! the process at startup on any CPU without AVX. With `ort`'s
//! `load-dynamic` feature ONNX is NOT statically linked: a shared library
//! (`.so`) is built from the prebuilt archive at compile time, shipped
//! beside the binary, and dlopen'd at runtime after an AVX check. Non-AVX
//! CPUs get a working binary minus OCR/rerank instead of a SIGILL.
//!
//! ## Linux aarch64 : no ONNX
//!
//! Released without `ocr,rerank`. ONNX Runtime's static global
//! constructors can deadlock there (issue #9), so the features are simply
//! not built rather than shipped broken.
//!
//! ## macOS : static link (arm64 only)
//!
//! arm64 links statically via `download-binaries`; there is no AVX concept
//! on ARM (NEON), so no gate is needed. x86_64-apple-darwin is released
//! without `ocr,rerank` because `ort-sys` publishes no prebuilt for that
//! target.
//!
//! ## Windows x64 : static link
//!
//! Statically linked via `download-binaries`. AVX issues are rare (most
//! x64 CPUs since 2011 have it), and dynamic loading is not an option
//! regardless: pyke ships **no `onnxruntime.dll` for Windows at all** :
//! the artifact is `onnxruntime.lib` (a ~305MB static archive) plus
//! `DirectML.dll`, nothing else. Even if a DLL existed, the MSVC linker
//! cannot build one from that archive because of duplicate protobuf
//! symbols. See the DirectML section below for what the static link drags
//! in.
//!
//! ## Postmortem : how the v3.3.0 feature leak stayed silent
//!
//! The rule itself is at the top of this comment; this is why nothing
//! caught the violation. `load-dynamic` implies `ort-sys/disable-linking`,
//! and `ort-sys`'s build script early-returns on that flag : before
//! downloading anything and before `copy-dylibs` runs : so there is no
//! build-time error, only a runtime dlopen that finds nothing. At runtime,
//! `load_and_init()` below still discards `commit()`'s `Result`, and the
//! doctor's "static link, compiled in" line is a `cfg` constant rather
//! than a probe, so neither surfaced it either. Worth fixing if you touch
//! this again. The tell in the shipped artifacts was the Windows exe
//! dropping 35.6MB -> 16.3MB : the missing ONNX static archive.
//!
//! ## Windows : `DirectML.dll` is linked and never called
//!
//! pyke's Windows archive is always built with the DirectML execution
//! provider, so `ort-sys` unconditionally emits `dxguid`, `DXCORE`,
//! `DXGI`, `D3D12` and `DirectML` link directives (see `ort-sys`
//! `build/static_link/mod.rs`). `donsetch.exe` therefore hard-imports
//! `DirectML.dll` by ordinal 2 (`DMLCreateDevice1`) and maps it at process
//! start : but never calls it: we register no execution providers, so ONNX
//! runs on the CPU provider. Cost is address space plus a `DllMain`, not
//! resident memory.
//!
//! This cannot be removed by features: `ort`'s `directml` feature maps to
//! `ort-sys`'s `directml = []`, which is empty and referenced nowhere in
//! its build scripts : it gates only the Rust-side EP API, not the
//! prebuilt. Dropping the dependency would mean building ONNX Runtime from
//! source without `--use_dml` and pointing `ORT_LIB_PATH` at it.
//!
//! Consequences worth knowing:
//!
//! - `DirectML.dll` is an in-box OS component from Windows 10 1903 (build
//!   18362) onward, so normal installs need nothing. Trimmed images
//!   (Server Core, Nano Server) and pre-1903 have no copy and the process
//!   dies at load; the fix is `bin/x64-win/DirectML.dll` from the
//!   `Microsoft.AI.DirectML` NuGet package placed beside the binary.
//! - The in-box **version is irrelevant**. DirectML's entire export surface
//!   has been two functions since 1.0, so Windows 10 22H2's 1.0.200713
//!   satisfies the import exactly as Windows 11's 1.15.x does (verified on
//!   both).
//! - **Never transplant a `System32` copy between Windows versions.** In-box
//!   builds are tied to their OS; a Windows 10 one dropped beside the binary
//!   on Windows 11 shadows the system copy (DirectML is not a KnownDLL, so
//!   the exe directory wins) and kills the process at load with
//!   `STATUS_DLL_INIT_FAILED` (`0xC0000142`) and no output.
//! - `copy-dylibs` stays enabled deliberately. It places the redist next to
//!   dev builds so they run on machines without an in-box copy, and it never
//!   reaches releases because the release workflow packages explicit
//!   filenames (`donsetch.exe`, `pdfium.dll`).
//! - `/DELAYLOAD:DirectML.dll` would work mechanically and would shed the
//!   dependency entirely, but was rejected: delay-load failures raise SEH,
//!   which Rust cannot catch, converting a deterministic load-time failure
//!   into an uncatchable runtime abort (`panic = "abort"`) if ONNX ever does
//!   reach for the provider.

#[cfg(all(target_os = "linux", any(feature = "ocr", feature = "rerank")))]
use std::path::PathBuf;
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::sync::OnceLock;
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(feature = "ocr", feature = "rerank"))]
use std::time::Duration;
/// Error returned when the CPU lacks AVX support (Linux only).
pub const NO_AVX_MSG: &str = "ONNX Runtime requires AVX CPU support. Your CPU does not support AVX (pre-2011 Intel or virtualized without AVX passthrough). OCR and rerank are disabled. All other features work normally.";

/// Message when an init attempt deadlocked inside the dynamic
/// loader (pykeio/ort #579/#560 class). Kept as a stable, actionable
/// line: the user can still run without OCR/rerank.
pub const ONNX_HUNG_MSG: &str = "ONNX Runtime initialization hung (known upstream loader deadlock); OCR and rerank are disabled for this run. Fetch, PDF, crawl and search all continue to work normally.";

/// How long the dedicated ONNX init thread may take before we
/// declare the loader deadlocked and fail fast.
pub const ONNX_INIT_TIMEOUT_SECS: u64 = 15;

/// Ensure ONNX Runtime is loaded and initialized.
///
/// Returns `Ok(())` if ONNX is ready for use, or an `Err` with a
/// human-readable message explaining why OCR/rerank is unavailable.
///
/// Safe to call multiple times: the first call loads+inits, all
/// subsequent calls return immediately.
pub fn ensure_loaded() -> Result<(), String> {
    #[cfg(not(any(feature = "ocr", feature = "rerank")))]
    {
        Err("not compiled with OCR/rerank support".to_string())
    }
    #[cfg(any(feature = "ocr", feature = "rerank"))]
    {
        static STATE: OnceLock<Result<(), String>> = OnceLock::new();
        static HUNG: AtomicBool = AtomicBool::new(false);

        if let Some(r) = STATE.get() {
            return r.clone();
        }
        // Once an init attempt has hung, fail fast forever: do not
        // re-spawn a thread that will also hang (each hung attempt
        // leaks that thread; a retry-happy daemon would stack them).
        if HUNG.load(Ordering::Acquire) {
            return Err(ONNX_HUNG_MSG.to_string());
        }

        // ort's init path (dlopen on Linux, env construction on
        // macOS/Windows) can deadlock inside the dynamic loader in
        // complex binaries (pykeio/ort #579, #560) instead of
        // returning an error. Run it on a dedicated thread with a
        // bounded wait so a hung loader can never hang the MCP
        // server; the stuck thread leaks (it cannot be killed) but
        // the daemon keeps working and every later call fails fast.
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("onnx-init".into())
            .spawn(move || {
                // Receiver may already be gone on timeout: ignore.
                let _ = tx.send(load_and_init());
            });
        let outcome = match spawned {
            Ok(_) => match rx.recv_timeout(Duration::from_secs(ONNX_INIT_TIMEOUT_SECS)) {
                Ok(r) => r,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    HUNG.store(true, Ordering::Release);
                    Err(ONNX_HUNG_MSG.to_string())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    Err("ONNX init thread panicked".to_string())
                }
            },
            Err(e) => Err(format!("cannot spawn ONNX init thread: {e}")),
        };
        STATE.get_or_init(|| outcome).clone()
    }
}

// ── Linux: dynamic loading via dlopen ───────────────────────────

#[cfg(all(target_os = "linux", any(feature = "ocr", feature = "rerank")))]
fn load_and_init() -> Result<(), String> {
    // 1. AVX gate (disk-cached, permanent if true).
    if !crate::cpu::has_avx() {
        return Err(NO_AVX_MSG.to_string());
    }

    // 2. Find the shared library.
    let lib_path = find_shared_lib().ok_or_else(|| {
        "ONNX Runtime shared library not found. \
            OCR and rerank are disabled."
            .to_string()
    })?;

    // 3. dlopen and init.
    //    ort::init_from loads the .so via libloading.
    //    builder.commit() initializes the ONNX environment.
    let builder = ort::init_from(&lib_path).map_err(|e| {
        format!(
            "Failed to load ONNX Runtime from {}: {e}",
            lib_path.display()
        )
    })?;

    // Surface commit() failures: a dylib that loads but cannot
    // initialize must fail OCR/rerank loudly, not silently degrade
    // (that exact silence hid the v3.3.0 feature leak).
    // NOTE: on the load-dynamic path commit() reports bool.
    if !builder.commit() {
        return Err("ONNX Runtime init failed (dynamic load)".to_string());
    }

    eprintln!("[onnx] Runtime loaded from {}", lib_path.display());
    Ok(())
}

/// Find the ONNX Runtime shared library (Linux only).
///
/// Searches:
/// 1. Next to the current executable (primary).
/// 2. `cache_dir()/onnx/` (fallback for relocatable installs).
#[cfg(all(target_os = "linux", any(feature = "ocr", feature = "rerank")))]
fn find_shared_lib() -> Option<PathBuf> {
    let lib_name = shared_lib_name();

    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join(lib_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    let cache = crate::paths::cache_dir().join("onnx").join(lib_name);
    if cache.exists() {
        return Some(cache);
    }

    None
}

/// Platform-specific shared library filename (Linux only).
#[cfg(all(target_os = "linux", any(feature = "ocr", feature = "rerank")))]
fn shared_lib_name() -> &'static str {
    "libonnxruntime.so"
}

// ── macOS / Windows: static linking ────────────────────────────

#[cfg(all(not(target_os = "linux"), any(feature = "ocr", feature = "rerank")))]
fn load_and_init() -> Result<(), String> {
    // macOS ARM64: no AVX concept (ARM NEON). Always works.
    // Windows x64: if no AVX, process already crashed at startup
    //   (static constructors ran before main). This code only
    //   runs on AVX-capable machines.
    // Just initialize the ONNX environment (static link).
    // Surface commit() failures: the 3.3.0 leak shipped binaries
    // where the static archive was never linked in and this call
    // failed silently : treat it as an error instead.
    // NOTE: commit() reports bool on this path too.
    if !ort::init().commit() {
        return Err("ONNX Runtime init failed (static)".to_string());
    }
    eprintln!("[onnx] Runtime initialized (static link)");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(all(target_os = "linux", any(feature = "ocr", feature = "rerank")))]
    #[test]
    fn shared_lib_name_is_so_on_linux() {
        assert_eq!(super::shared_lib_name(), "libonnxruntime.so");
    }

    /// Payload probe: the ONNX environment must actually initialize
    /// in this binary. On static-link targets this is the only thing
    /// that catches a build where the archive was never linked in
    /// (the v3.3.0 leak); on Linux it proves the dylib loads and
    /// commits. Runs in every features-enabled CI job, so a dead
    /// payload fails at merge time, not at release time. Non-AVX
    /// Linux hosts skip it: they can't run ONNX by design (their
    /// builds must still pass).
    #[cfg(any(feature = "ocr", feature = "rerank"))]
    #[test]
    fn onnx_payload_probe_initializes() {
        #[cfg(target_os = "linux")]
        if !crate::cpu::has_avx() {
            eprintln!("skipping ONNX payload probe: host has no AVX");
            return;
        }
        super::ensure_loaded().expect("ONNX Runtime failed to initialize");
        // A second call must reuse the memoized state, not re-init.
        super::ensure_loaded().expect("ONNX Runtime failed to initialize (recheck)");
    }
}
