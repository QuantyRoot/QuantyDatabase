//! Replacing the tool with a newer copy of itself.
//!
//! Two halves, and only one of them is here. Getting a release is a
//! network problem: it needs HTTPS, and this project writes what it
//! depends on (ADR-020), so it needs TLS first. Installing one is a file
//! problem, and that half works today.
//!
//! So `update --file` takes a binary that is already on the machine and
//! puts it where the running one is. When TLS lands, fetching produces a
//! file and hands it to exactly this code: the download changes, the
//! install does not.
//!
//! ## What it checks before it replaces anything
//!
//! A self-updater that writes first and asks later is a way to turn a
//! truncated download into an unusable machine, so nothing is replaced
//! until the replacement has proven it works:
//!
//! - the candidate is copied next to the target, on the same filesystem,
//!   so the final step can be a rename and not a copy
//! - the copy is made executable and **run**, and has to answer `about`
//!   with a version. A truncated file, a text file, a binary for another
//!   architecture and an empty file all fail here rather than later
//! - its checksum is printed, and checked when one was given
//! - the version it reports is compared with this one, and going backwards
//!   or sideways is said out loud rather than assumed to be a mistake
//!
//! Only then does the old binary move aside and the new one take its
//! place. The old one stays as `.old`, because the fastest way back from
//! a bad update should not involve a download.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use quantydb_core::{sha256, to_hex};

use crate::{emit, failed, usage, Failure};

/// Install `candidate` over the running binary.
pub fn update(candidate: Option<&str>, expected: Option<&str>, yes: bool) -> Result<(), Failure> {
    let Some(candidate) = candidate else {
        return Err(usage(
            "update needs --file <binary> for now. Fetching a release over \
             the network needs TLS, which is written here rather than pulled \
             in and is not written yet, so point this at a file you have \
             already downloaded.",
        ));
    };
    let candidate = Path::new(candidate);

    let target = env::current_exe()
        .map_err(|e| failed(format!("cannot tell where this binary lives: {e}")))?;
    let directory = target
        .parent()
        .ok_or_else(|| failed(format!("{} has no directory", target.display())))?;

    let bytes =
        fs::read(candidate).map_err(|e| failed(format!("reading {}: {e}", candidate.display())))?;
    if bytes.is_empty() {
        return Err(failed(format!("{} is empty", candidate.display())));
    }
    let digest = to_hex(&sha256(&bytes));

    // Before anything is written: does the file agree with itself about how
    // long it is? A cut download keeps its header and loses its tail, and
    // running it would not notice.
    if let Some(claimed) = whole::claimed_length(&bytes) {
        let actual = bytes.len() as u64;
        if actual < claimed {
            return Err(failed(format!(
                "{} is incomplete: its own headers describe {claimed} bytes \
                 and the file is {actual}. That is what half a download \
                 looks like; fetch it again.",
                candidate.display()
            )));
        }
    }

    if let Some(want) = expected {
        let want = want.trim().to_ascii_lowercase();
        if want != digest {
            return Err(failed(format!(
                "checksum does not match\n  wanted {want}\n  got    {digest}"
            )));
        }
    }

    // Next to the target rather than in a temporary directory, because the
    // last step has to be a rename and a rename does not cross filesystems.
    let staged = beside(&target, "new")?;
    let previous = beside(&target, "old")?;
    let _ = fs::remove_file(&staged);

    write_program(&staged, &bytes).map_err(|e| {
        let _ = fs::remove_file(&staged);
        failed(format!(
            "cannot write to {}: {e}\nThe binary lives somewhere this user \
             cannot write. Run this again with the rights to change it, or \
             install into a directory you own.",
            directory.display()
        ))
    })?;

    let found = match version_of(&staged) {
        Ok(version) => version,
        Err(e) => {
            let _ = fs::remove_file(&staged);
            return Err(e);
        }
    };

    let running = env!("CARGO_PKG_VERSION");
    emit(&format!("  from  {running}"))?;
    emit(&format!("  to    {found}"))?;
    emit(&format!("  sha   {digest}"))?;
    if expected.is_none() {
        emit("  (no --sha256 given, so that checksum is what arrived, not")?;
        emit("   what was published)")?;
    }
    if found == running {
        emit("  note: that is the version already running")?;
    } else if older(&found, running) {
        emit("  note: that is older than the version already running")?;
    }

    if !yes && !confirmed()? {
        let _ = fs::remove_file(&staged);
        return emit("left alone");
    }

    let _ = fs::remove_file(&previous);
    fs::rename(&target, &previous).map_err(|e| {
        let _ = fs::remove_file(&staged);
        failed(format!("cannot move the running binary aside: {e}"))
    })?;
    if let Err(e) = fs::rename(&staged, &target) {
        // Put it back rather than leaving nothing where the tool was.
        let _ = fs::rename(&previous, &target);
        let _ = fs::remove_file(&staged);
        return Err(failed(format!("cannot put the new binary in place: {e}")));
    }

    emit(&format!("{} is now {found}", target.display()))?;
    emit(&format!("the old one is {}", previous.display()))
}

/// `quantydb` -> `quantydb.new`, and `quantydb.exe` -> `quantydb.new.exe`.
///
/// The suffix matters: Windows will not run a file that does not end in
/// `.exe`, and this one gets run before it is installed.
fn beside(target: &Path, what: &str) -> Result<PathBuf, Failure> {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| failed(format!("{} has an odd name", target.display())))?;
    let stem = name.strip_suffix(env::consts::EXE_SUFFIX).unwrap_or(name);
    let directory = target
        .parent()
        .ok_or_else(|| failed(format!("{} has no directory", target.display())))?;
    Ok(directory.join(format!("{stem}.{what}{}", env::consts::EXE_SUFFIX)))
}

/// Write the bytes and make them runnable.
fn write_program(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    executable(path)
}

#[cfg(unix)]
fn executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// How long the file claims to be, read out of its own headers.
///
/// Running a candidate proves it starts. It does not prove it is whole:
/// a binary cut to a third of its length still has an intact header and
/// still answers `about`, because nothing on that path reaches a page
/// that is no longer there. The truncation surfaces later, as a crash in
/// whichever command first touches the missing part.
///
/// Every one of these formats records where its own last byte should be,
/// so the file can be measured against what it says about itself. An
/// unknown format answers `None` and is not refused: not being able to
/// check is not the same as having found something wrong.
mod whole {
    /// The offset one past the last byte any header points at.
    pub fn claimed_length(bytes: &[u8]) -> Option<u64> {
        match bytes.get(..4)? {
            [0x7f, b'E', b'L', b'F'] => elf(bytes),
            [0xcf, 0xfa, 0xed, 0xfe] => macho(bytes),
            [b'M', b'Z', ..] => pe(bytes),
            _ => None,
        }
    }

    fn u16_at(bytes: &[u8], at: usize) -> Option<u64> {
        let raw: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
        Some(u16::from_le_bytes(raw) as u64)
    }

    fn u32_at(bytes: &[u8], at: usize) -> Option<u64> {
        let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(u32::from_le_bytes(raw) as u64)
    }

    fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
        let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
        Some(u64::from_le_bytes(raw))
    }

    /// 64-bit ELF: the section table, and every program header's extent.
    fn elf(bytes: &[u8]) -> Option<u64> {
        if bytes.get(4) != Some(&2) {
            return None; // 32-bit, which nothing here ships
        }
        let mut end = 0u64;

        let shoff = u64_at(bytes, 0x28)?;
        let shentsize = u16_at(bytes, 0x3a)?;
        let shnum = u16_at(bytes, 0x3c)?;
        end = end.max(shoff.checked_add(shentsize.checked_mul(shnum)?)?);

        let phoff = u64_at(bytes, 0x20)?;
        let phentsize = u16_at(bytes, 0x36)?;
        let phnum = u16_at(bytes, 0x38)?;
        for i in 0..phnum {
            let at = usize::try_from(phoff.checked_add(i.checked_mul(phentsize)?)?).ok()?;
            let offset = u64_at(bytes, at + 0x08)?;
            let size = u64_at(bytes, at + 0x20)?;
            end = end.max(offset.checked_add(size)?);
        }
        Some(end)
    }

    /// 64-bit Mach-O: the extent of every `LC_SEGMENT_64`.
    ///
    /// `struct segment_command_64` counts from the start of the command:
    /// `cmd` 0, `cmdsize` 4, `segname[16]` 8, `vmaddr` 24, `vmsize` 32,
    /// `fileoff` 40, `filesize` 48. Reading 32 and 40 instead picks up
    /// `vmsize` and `fileoff`, which on `__PAGEZERO` is four gibibytes of
    /// address space that is in no file, and every macOS binary then looks
    /// truncated. The macOS runner is what caught that.
    fn macho(bytes: &[u8]) -> Option<u64> {
        const LC_SEGMENT_64: u64 = 0x19;
        let ncmds = u32_at(bytes, 0x10)?;
        let mut at = 0x20usize;
        let mut end = 0u64;
        for _ in 0..ncmds {
            let kind = u32_at(bytes, at)?;
            let size = u32_at(bytes, at + 4)?;
            if size == 0 {
                return None;
            }
            if kind == LC_SEGMENT_64 {
                let fileoff = u64_at(bytes, at + 40)?;
                let filesize = u64_at(bytes, at + 48)?;
                end = end.max(fileoff.checked_add(filesize)?);
            }
            at = at.checked_add(usize::try_from(size).ok()?)?;
        }
        Some(end)
    }

    /// PE: the extent of every section's raw data.
    fn pe(bytes: &[u8]) -> Option<u64> {
        let lfanew = usize::try_from(u32_at(bytes, 0x3c)?).ok()?;
        if bytes.get(lfanew..lfanew + 4)? != b"PE\0\0" {
            return None;
        }
        let sections = u16_at(bytes, lfanew + 6)?;
        let optional = u16_at(bytes, lfanew + 20)?;
        let table = lfanew
            .checked_add(24)?
            .checked_add(usize::try_from(optional).ok()?)?;
        let mut end = 0u64;
        for i in 0..sections {
            let at = table.checked_add(usize::try_from(i.checked_mul(40)?).ok()?)?;
            let size = u32_at(bytes, at + 16)?;
            let offset = u32_at(bytes, at + 20)?;
            end = end.max(offset.checked_add(size)?);
        }
        Some(end)
    }
}

/// Run the candidate and make it say who it is.
///
/// This catches what a length check cannot: a whole file that is the wrong
/// program, a binary for another architecture, an error page saved under
/// the right name. It does not catch truncation, which is what `whole`
/// is for, and finding that out was the point of the test that cuts a
/// binary to a third and watches it answer `about` quite happily.
fn version_of(program: &Path) -> Result<String, Failure> {
    let out = Command::new(program).arg("about").output().map_err(|e| {
        failed(format!(
            "{} does not run on this machine: {e}",
            program.display()
        ))
    })?;
    if !out.status.success() {
        return Err(failed(format!(
            "{} ran and failed, so it is not a working quantydb",
            program.display()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("").trim();
    let Some(version) = first.strip_prefix("quantydb ") else {
        return Err(failed(format!(
            "{} is not quantydb: it answered {first:?}",
            program.display()
        )));
    };
    let version = version.trim();
    if version.is_empty() {
        return Err(failed("the candidate did not say which version it is"));
    }
    Ok(version.to_string())
}

/// Whether `found` sorts before `running`, comparing numbers as numbers.
///
/// Anything that does not parse is treated as not older, because refusing
/// to install something whose version this cannot read would be worse than
/// installing it.
fn older(found: &str, running: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|p| p.parse::<u64>().ok())
            .collect()
    };
    let (a, b) = (parts(found), parts(running));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a < b
}

/// Ask, on the terminal, and take only yes for an answer.
fn confirmed() -> Result<bool, Failure> {
    print!("replace it? [y/N] ");
    std::io::stdout()
        .flush()
        .map_err(|e| failed(format!("stdout: {e}")))?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| failed(format!("stdin: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::older;

    #[test]
    fn versions_compare_as_numbers_and_not_as_text() {
        assert!(older("0.9.0", "0.10.0"), "text would call 0.9 the newer");
        assert!(older("0.3.0", "0.4.0"));
        assert!(!older("0.4.0", "0.3.0"));
        assert!(!older("0.4.0", "0.4.0"), "the same is not older");
        assert!(older("0.4.0", "0.4.1"));
    }

    /// A version this cannot read is not a reason to refuse the update.
    #[test]
    fn unreadable_versions_are_not_called_older() {
        assert!(!older("", "0.4.0"));
        assert!(!older("nightly", "0.4.0"));
        assert!(!older("0.4.0", "whatever"));
    }
}
