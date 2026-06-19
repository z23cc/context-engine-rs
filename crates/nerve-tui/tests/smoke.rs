//! Integration smoke test: spawn a real `nerve daemon --stdio` against a temp
//! root, run the runtime/info handshake, and round-trip a `ping` job to a
//! completed status. Mirrors `bun run tui:smoke`.
//!
//! Requires a built `nerve` binary. We locate `target/<profile>/nerve` relative
//! to this test's own executable (so it works under `cargo test` regardless of
//! the cwd); if it isn't there, the test fails with a clear "build it first"
//! message rather than silently passing.

use std::path::PathBuf;

use nerve_runtime::RuntimeJobStatus;
use nerve_tui::smoke::{run_smoke, smoke_spec};

#[tokio::test]
async fn smoke_ping_round_trip() {
    let binary = locate_nerve_binary().expect(
        "could not find the `nerve` binary — run `cargo build -p nerve-workstation --bin nerve` \
         first (looked next to the test executable in target/<profile>/)",
    );
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("notes.txt"), "hello nerve\n").expect("seed file");

    let spec = smoke_spec(temp.path().to_path_buf(), Some(binary));
    let report = run_smoke(spec).await.expect("smoke round-trip");

    assert_eq!(report.job_status, RuntimeJobStatus::Completed);
    assert!(report.tools > 0, "expected the daemon to advertise tools");
    println!("{}", report.pass_line());
}

/// Find the `nerve` binary next to this test's executable. Cargo places test
/// binaries in `target/<profile>/deps/`, and `nerve` in `target/<profile>/`, so
/// walk up to two parents and probe.
fn locate_nerve_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) { "nerve.exe" } else { "nerve" };
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent();
    for _ in 0..3 {
        let candidate = dir?.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir?.parent();
    }
    None
}
