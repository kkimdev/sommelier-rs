use std::path::PathBuf;
use std::process::Command;

/// Run the real sample GUI through the GPU-enabled proxy.
///
/// This is ignored by default because it needs a live Wayland compositor and
/// a Crostini VirtWL device (or `SOMMELIER_GUI_SMOKE_COMPOSITOR`). Build both
/// binaries first, then run:
///
/// ```text
/// cargo test -p sommelier --test gui_smoke -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires a live Wayland compositor and locally built GUI binaries"]
fn sample_gui_runs_through_gpu_accel_proxy() {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/sommelier-gui-smoke.sh");
    // Test the proxy Cargo built for this test run, not whichever build the
    // script would find under target/.
    let status = Command::new(&script)
        .env("SOMMELIER_BIN", env!("CARGO_BIN_EXE_sommelier"))
        .env("SOMMELIER_GUI_SMOKE_KEEP_LOGS", "1")
        .status()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", script.display()));
    assert!(
        status.success(),
        "{} exited with {status}",
        script.display()
    );
}
