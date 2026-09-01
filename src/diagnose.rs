use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateMutexW, ReleaseMutex, WaitForSingleObject, INFINITE,
};

/// Rotate before a write would grow the current log past this size; one
/// previous generation is kept as `diagnose.log.old`.
const MAX_LOG_BYTES: u64 = 1_000_000;
const FILE_ATTRIBUTE_REPARSE_POINT_VALUE: u32 = 0x0000_0400;
/// Base name of the mutex that serializes writes to one log file.
///
/// The file lives under `%LOCALAPPDATA%`, so it is per user, while a `Global\`
/// name is machine wide. Two consequences, both fixed by deriving the name
/// from the path: unrelated users no longer wait on each other, and a user who
/// cannot create objects in the global namespace - that needs
/// `SeCreateGlobalPrivilege`, which an ordinary interactive account does not
/// have - falls back to the session namespace instead of losing diagnostics
/// altogether.
const DIAGNOSTIC_LOG_MUTEX_BASE: &str = "Gengchou-DiagnosticLog-v2";

struct DiagnoseState {
    path: PathBuf,
    local_writer: Mutex<()>,
    cross_process_mutex: HANDLE,
}

// Windows synchronization handles may be waited on and released from any
// thread. `local_writer` serializes this process and the named mutex
// serializes all cooperating processes before the handle is used.
unsafe impl Send for DiagnoseState {}
unsafe impl Sync for DiagnoseState {}

impl Drop for DiagnoseState {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.cross_process_mutex);
        }
    }
}

impl DiagnoseState {
    fn write_line(&self, line: &str) -> Result<(), String> {
        let _local_writer = self
            .local_writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _cross_process_writer = acquire_log_mutex(self.cross_process_mutex)?;
        write_log_line_at_path(&self.path, line)
    }
}

struct LogMutexGuard(HANDLE);

impl Drop for LogMutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
        }
    }
}

static DIAGNOSE_STATE: OnceLock<DiagnoseState> = OnceLock::new();

fn log_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "LOCALAPPDATA is unavailable; diagnostic logging was refused.".to_string()
        })?;
    if !base.is_absolute()
        || base
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(format!(
            "LOCALAPPDATA is not a clean absolute path: {}",
            base.display()
        ));
    }
    Ok(base.join("Gengchou").join("diagnose.log"))
}

/// The mutex names to try, most to least privileged.
///
/// `Global\` first so two sessions of the same account still serialize on one
/// file; `Local\` as the fallback, which covers a session that may not create
/// global objects. The suffix is the log path, lowercased before hashing
/// because Windows paths compare case-insensitively.
fn log_mutex_names(path: &Path) -> [String; 2] {
    let key = path.to_string_lossy().to_lowercase();
    let digest = crate::updater::sha256_hex(key.as_bytes()).unwrap_or_default();
    let short = digest.get(..16).unwrap_or("unhashed");
    [
        format!("Global\\{DIAGNOSTIC_LOG_MUTEX_BASE}-{short}"),
        format!("Local\\{DIAGNOSTIC_LOG_MUTEX_BASE}-{short}"),
    ]
}

fn create_log_mutex(path: &Path) -> Result<HANDLE, String> {
    let mut last_error = String::from("no mutex name was tried");
    for name in log_mutex_names(path) {
        let wide: Vec<u16> = OsStr::new(&name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        match unsafe { CreateMutexW(None, false, PCWSTR::from_raw(wide.as_ptr())) } {
            Ok(handle) => return Ok(handle),
            Err(error) => last_error = format!("{name}: {error}"),
        }
    }
    Err(format!(
        "Unable to create diagnostic log mutex ({last_error})"
    ))
}

fn acquire_log_mutex(handle: HANDLE) -> Result<LogMutexGuard, String> {
    let result = unsafe { WaitForSingleObject(handle, INFINITE) };
    if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
        Ok(LogMutexGuard(handle))
    } else {
        Err(format!(
            "Unable to acquire diagnostic log mutex: wait result {result:?}"
        ))
    }
}

pub fn init() -> Result<PathBuf, String> {
    if let Some(state) = DIAGNOSE_STATE.get() {
        return Ok(state.path.clone());
    }

    let path = log_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "Diagnostic log path has no parent directory.".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Unable to create diagnostic directory {}: {error}",
            parent.display()
        )
    })?;
    validate_regular_directory(parent)?;

    let state = DiagnoseState {
        path: path.clone(),
        local_writer: Mutex::new(()),
        cross_process_mutex: create_log_mutex(&path)?,
    };
    state.write_line(&format!(
        "[{} pid={}] --- diagnostic logging started v{} ---\n",
        timestamp(),
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    ))?;

    // A concurrent second init can only have installed an equivalent state.
    // Dropping this one closes its extra handle while preserving the winner.
    let _ = DIAGNOSE_STATE.set(state);
    Ok(path)
}

fn validate_regular_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Unable to inspect {}: {error}", path.display()))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
    {
        return Err(format!(
            "Refusing non-directory or reparse-point diagnostic path {}.",
            path.display()
        ));
    }
    Ok(())
}

fn regular_file_length_if_exists(path: &Path) -> Result<Option<u64>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Unable to inspect {}: {error}", path.display())),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
    {
        return Err(format!(
            "Refusing non-regular diagnostic file {}.",
            path.display()
        ));
    }
    Ok(Some(metadata.len()))
}

fn validate_opened_log(file: &File, path: &Path) -> Result<(), String> {
    let metadata = file.metadata().map_err(|error| {
        format!(
            "Unable to verify opened diagnostic log {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0 {
        return Err(format!(
            "Refusing non-regular diagnostic log {}.",
            path.display()
        ));
    }
    Ok(())
}

fn rotate_log(path: &Path) -> Result<(), String> {
    let old = path.with_extension("log.old");
    let _ = regular_file_length_if_exists(&old)?;
    match std::fs::remove_file(&old) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Unable to remove rotated diagnostic log {}: {error}",
                old.display()
            ))
        }
    }
    std::fs::rename(path, &old).map_err(|error| {
        format!(
            "Unable to rotate diagnostic log {}: {error}",
            path.display()
        )
    })
}

/// Caller must hold both the in-process writer lock and the named mutex.
fn write_log_line_at_path(path: &Path, line: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Diagnostic log path has no parent directory.".to_string())?;
    validate_regular_directory(parent)?;

    let current_length = regular_file_length_if_exists(path)?.unwrap_or(0);
    let incoming_length = u64::try_from(line.len()).unwrap_or(u64::MAX);
    if current_length > 0 && current_length.saturating_add(incoming_length) > MAX_LOG_BYTES {
        rotate_log(path)?;
    }

    // Reopen the current pathname for every write. If another process or a
    // support tool rotated it, this prevents a long-lived handle from
    // continuing to append to the renamed `.old` file.
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("Unable to open diagnostic log {}: {error}", path.display()))?;
    validate_opened_log(&file, path)?;
    file.write_all(line.as_bytes())
        .map_err(|error| format!("Unable to write diagnostic log {}: {error}", path.display()))?;
    file.flush()
        .map_err(|error| format!("Unable to flush diagnostic log {}: {error}", path.display()))
}

fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn log(message: impl AsRef<str>) {
    let Some(state) = DIAGNOSE_STATE.get() else {
        return;
    };
    let _ = state.write_line(&format!(
        "[{} pid={}] {}\n",
        timestamp(),
        std::process::id(),
        message.as_ref()
    ));
}

pub fn log_error(context: &str, error: impl std::fmt::Display) {
    log(format!("{context}: {error}"));
}

#[cfg(test)]
mod tests {
    /// The log file is per user, so the lock that serializes writes to it must
    /// be too. A single machine-wide name made unrelated accounts wait on each
    /// other and, worse, gave a session that may not create global objects no
    /// lock at all - diagnostics were then lost, silently, which is the failure
    /// mode this log exists to expose.
    #[test]
    fn the_log_mutex_is_scoped_to_the_log_file() {
        let mine = log_mutex_names(Path::new(
            r"C:\Users\alice\AppData\Local\Gengchou\diagnose.log",
        ));
        let theirs = log_mutex_names(Path::new(
            r"C:\Users\bob\AppData\Local\Gengchou\diagnose.log",
        ));
        assert_ne!(mine[0], theirs[0], "two users must not share one lock");

        // Global first so two sessions of the same account still serialize.
        assert!(mine[0].starts_with(r"Global\"));
        assert!(mine[1].starts_with(r"Local\"));
        assert_eq!(
            mine[0].trim_start_matches(r"Global\"),
            mine[1].trim_start_matches(r"Local\"),
            "the fallback must lock the same thing, in a different namespace"
        );

        // Windows paths compare case-insensitively; the name must agree.
        let shouted = log_mutex_names(Path::new(
            r"C:\USERS\ALICE\AppData\Local\Gengchou\DIAGNOSE.LOG",
        ));
        assert_eq!(mine, shouted);
    }

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            loop {
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "gengchou-diagnose-test-{}-{name}-{id}",
                    std::process::id()
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("unable to create test directory: {error}"),
                }
            }
        }

        fn log_path(&self) -> PathBuf {
            self.0.join("diagnose.log")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn runtime_write_rotates_before_crossing_the_limit() {
        let directory = TestDirectory::new("runtime-rotation");
        let path = directory.log_path();
        let old = path.with_extension("log.old");
        let original = vec![b'x'; MAX_LOG_BYTES as usize - 2];
        std::fs::write(&path, &original).expect("large current log");

        write_log_line_at_path(&path, "new\n").expect("rotating write");

        assert_eq!(std::fs::read(&old).expect("rotated log"), original);
        assert_eq!(
            std::fs::read_to_string(&path).expect("current log"),
            "new\n"
        );
    }

    #[test]
    fn reopening_each_write_follows_an_external_rotation() {
        let directory = TestDirectory::new("external-rotation");
        let path = directory.log_path();
        let external = directory.0.join("external.log");
        write_log_line_at_path(&path, "first\n").expect("first write");
        std::fs::rename(&path, &external).expect("external rotation");
        std::fs::write(&path, "replacement\n").expect("external replacement");

        write_log_line_at_path(&path, "second\n").expect("second write");

        assert_eq!(
            std::fs::read_to_string(&external).expect("external archive"),
            "first\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("replacement current log"),
            "replacement\nsecond\n"
        );
    }

    #[test]
    fn repeated_runtime_rotation_keeps_only_current_and_one_old_generation() {
        let directory = TestDirectory::new("two-generations");
        let path = directory.log_path();
        let old = path.with_extension("log.old");
        std::fs::write(&old, "stale generation\n").expect("stale old log");

        std::fs::write(&path, vec![b'a'; MAX_LOG_BYTES as usize])
            .expect("first oversized current log");
        write_log_line_at_path(&path, "first\n").expect("first rotation");
        std::fs::write(&path, vec![b'b'; MAX_LOG_BYTES as usize])
            .expect("second oversized current log");
        write_log_line_at_path(&path, "second\n").expect("second rotation");

        let mut names = std::fs::read_dir(&directory.0)
            .expect("diagnostic directory")
            .map(|entry| entry.expect("diagnostic entry").file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            vec![
                OsStr::new("diagnose.log").to_os_string(),
                OsStr::new("diagnose.log.old").to_os_string(),
            ]
        );
        assert_eq!(std::fs::read(&old).expect("latest old log")[0], b'b');
        assert_eq!(
            std::fs::read_to_string(&path).expect("latest current log"),
            "second\n"
        );
    }
}
