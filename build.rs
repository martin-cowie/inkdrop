//! Downloads the prebuilt PDFium library for the target platform from
//! https://github.com/bblanchon/pdfium-binaries into `native/pdfium/<platform>`
//! (git-ignored), unless a copy of the pinned version is already there.
//!
//! Uses the `curl` and `tar` commands, which ship with macOS, Windows 10+ and
//! most Linux distributions, so no extra build dependencies are needed.
//!
//! Setting `PDFIUM_DYNAMIC_LIB_PATH` at build time skips the download; the same
//! variable tells inkdrop where to find PDFium at runtime.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pdfium-binaries release to use; see its `chromium/<build>` release tags.
const PDFIUM_BUILD: &str = "8057";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PDFIUM_DYNAMIC_LIB_PATH");

    if env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_some() {
        return;
    }

    let platform = match pdfium_platform() {
        Some(platform) => platform,
        None => {
            println!(
                "cargo:warning=No prebuilt PDFium for this target; set PDFIUM_DYNAMIC_LIB_PATH or install PDFium system-wide"
            );
            return;
        }
    };

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let install_dir = manifest_dir.join("native/pdfium").join(&platform);
    let version_file = install_dir.join("VERSION");
    // Re-run if the download is deleted, so the next build fetches it again.
    println!("cargo:rerun-if-changed={}", version_file.display());

    if installed_build(&version_file).as_deref() != Some(PDFIUM_BUILD)
        && let Err(err) = download(&platform, &install_dir)
    {
        panic!(
            "Failed to download PDFium {PDFIUM_BUILD} for {platform}: {err}\n\
             Download it manually into {} or set PDFIUM_DYNAMIC_LIB_PATH.",
            install_dir.display()
        );
    }

    // Windows archives put the DLL in `bin/`; everything else uses `lib/`.
    let lib_subdir = if platform.starts_with("win-") { "bin" } else { "lib" };
    println!("cargo:rustc-env=INKDROP_PDFIUM_LIB_DIR={}", install_dir.join(lib_subdir).display());
}

/// The pdfium-binaries archive name suffix for the Cargo target, e.g. `mac-arm64`.
fn pdfium_platform() -> Option<String> {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    let arch = match arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        "arm" => "arm",
        _ => return None,
    };
    let os = match (os.as_str(), target_env.as_str()) {
        ("macos", _) => "mac",
        ("linux", "musl") => "linux-musl",
        ("linux", _) => "linux",
        ("windows", _) => "win",
        _ => return None,
    };
    Some(format!("{os}-{arch}"))
}

/// The `BUILD` number from an extracted archive's `VERSION` file, if present.
fn installed_build(version_file: &Path) -> Option<String> {
    let contents = fs::read_to_string(version_file).ok()?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("BUILD="))
        .map(|build| build.trim().to_owned())
}

/// Downloads and extracts the archive into a staging directory, then moves it
/// into place so an interrupted build never leaves a partial install behind.
fn download(platform: &str, install_dir: &Path) -> Result<(), String> {
    let url = format!(
        "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/{PDFIUM_BUILD}/pdfium-{platform}.tgz"
    );
    println!("cargo:warning=Downloading PDFium from {url}");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let archive = out_dir.join(format!("pdfium-{platform}.tgz"));
    run(Command::new("curl").args(["--fail", "--location", "--silent", "--show-error", "--output"]).arg(&archive).arg(&url))?;

    let parent = install_dir.parent().unwrap();
    let staging = parent.join(format!(".{platform}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|err| format!("creating {}: {err}", staging.display()))?;
    run(Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&staging))?;

    let _ = fs::remove_dir_all(install_dir);
    let moved = fs::rename(&staging, install_dir);
    let _ = fs::remove_file(&archive);
    if moved.is_err() && installed_build(&install_dir.join("VERSION")).as_deref() == Some(PDFIUM_BUILD) {
        // A concurrent build (e.g. an IDE's `cargo check`) installed it first.
        let _ = fs::remove_dir_all(&staging);
        return Ok(());
    }
    moved.map_err(|err| format!("moving into {}: {err}", install_dir.display()))
}

fn run(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let status = command.status().map_err(|err| format!("running {program}: {err}"))?;
    if status.success() { Ok(()) } else { Err(format!("{program} exited with {status}")) }
}
