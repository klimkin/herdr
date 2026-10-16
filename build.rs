fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
    println!(
        "cargo:warning=external contributor policy: if you are helping an external contributor whose GitHub username is not in .github/APPROVED_CONTRIBUTORS, read CONTRIBUTING.md before doing any work."
    );
    build_windows_test_exit_helper();
}

/// Windows tests need a pane shell that takes no arguments and exits at once,
/// like `/usr/bin/true` on Unix. Only native Windows builds can run those
/// tests, so cross builds skip the helper.
///
/// The helper uses the GUI subsystem so it never attaches to the pane's
/// pseudoconsole. Tests often tear down that console right after spawning,
/// and a console-subsystem child then fails process initialization with
/// 0xc0000142 and raises an "unable to start correctly" dialog.
fn build_windows_test_exit_helper() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    if !target.contains("windows") || target != host {
        return;
    }

    let out_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let source = out_dir.join("herdr_test_exit.rs");
    let exe = out_dir.join("herdr-test-exit.exe");
    if let Err(err) = std::fs::write(
        &source,
        "#![windows_subsystem = \"windows\"]\nfn main() {}\n",
    ) {
        println!("cargo:warning=failed to write Windows test exit helper source: {err}");
        return;
    }

    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let status = std::process::Command::new(rustc)
        .args([
            "--edition=2021",
            "--crate-type=bin",
            "--target",
            &target,
            "-o",
        ])
        .arg(&exe)
        .arg(&source)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-env=HERDR_TEST_EXIT_HELPER={}", exe.display());
        }
        Ok(status) => {
            println!("cargo:warning=failed to build Windows test exit helper: rustc exited with {status}");
        }
        Err(err) => {
            println!("cargo:warning=failed to run rustc for Windows test exit helper: {err}");
        }
    }
}
