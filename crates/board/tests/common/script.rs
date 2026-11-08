//! Fake executables for tests. Both the integration tests (through `common`) and the
//! library's unit tests (through `#[path]` in `lib.rs`) include this file.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `text` to `path` as an executable (mode 755) script.
///
/// A child `sh` writes the file, so this process never holds a write fd to it. On Linux,
/// a test thread that forks while this process holds such a fd gives the fork a copy until
/// the fork execs. Running the script in that window fails with ETXTBSY ("Text file busy").
/// Renaming the file afterwards does not help: the fork's fd still points at the inode.
pub fn write_script(path: &Path, text: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin.write_all(text.as_bytes()).unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "writing {}: {status}", path.display());
}
