//! Shared helpers for the integration tests (mirrors the core copy).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A directory under the system temp dir, removed on drop.
///
/// This is the one thing these tests ever used the tempfile crate for.
/// Owning these thirty lines keeps the workspace free of external dev
/// dependencies, which is what lets CI run the full test suite on the
/// MSRV toolchain: dev dependency trees adopt new language editions on
/// their own schedule, not ours (see ADR-013).
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new() -> TestDir {
        // pid + clock nanos + counter: unique across concurrent test
        // binaries, across threads within one binary, and across reruns
        // that might reuse a pid.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before the unix epoch")
            .as_nanos();
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quantydb-test-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        std::fs::create_dir(&path).expect("create test dir");
        TestDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for TestDir {
    fn default() -> Self {
        TestDir::new()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // Best effort, never panic in drop. A SIGKILLed test run leaks
        // its directory, the same trade-off tempfile makes.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Run a program the test just wrote, retrying while the kernel says the
/// file is busy.
///
/// Copying a binary and then executing it races with every other test in
/// the same run. `fork` hands a child every descriptor the parent had
/// open, including one another test is still writing its own copy
/// through, and the kernel refuses to execute a file that anybody holds
/// open for writing: ETXTBSY, `ExecutableFileBusy`. The window is between
/// that fork and its exec, so it is short, it is nobody's bug, and it
/// only ever appears on a machine with enough cores to run the tests at
/// once. It appeared on CI and never here.
///
/// Retrying is the whole fix. Failing for any other reason fails now.
#[allow(dead_code)]
pub fn run_tool(command: &mut std::process::Command) -> std::process::Output {
    use std::io::ErrorKind;
    use std::thread::sleep;
    use std::time::Duration;

    let mut waited = Duration::ZERO;
    let step = Duration::from_millis(20);
    loop {
        match command.output() {
            Ok(out) => return out,
            Err(e)
                if e.kind() == ErrorKind::ExecutableFileBusy
                    && waited < Duration::from_secs(10) =>
            {
                sleep(step);
                waited += step;
            }
            Err(e) => panic!("the tool did not run: {e}"),
        }
    }
}
