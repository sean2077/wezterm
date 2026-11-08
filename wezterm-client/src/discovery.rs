use anyhow::Context;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use wezterm_uds::UnixStream;

/// There's a lot more code in this windows module than I thought I would need
/// to write.  Ostensibly, we could get away with making a symlink by taking
/// the SessionName environment variable, combining it with the class name
/// and using a symlink to point to the actual path.
/// Symlinks are problematic on Windows, and the SessionName environment
/// variable may not be set.
/// It's a bit of a chore to resolve the name, and then it would be more
/// of a chore to manage the symlink.
/// What this module does is logically equivalent to the above, except
/// that it creates a piece of shared memory in the per-desktop namespace.
/// While there is a lot of code in here, it is simpler overall because
/// the naming is managed by the OS, as well as automatically removing
/// the name from the namespace when there are no more references to it.
#[cfg(windows)]
mod windows {
    use super::*;
    use std::io::Error as IoError;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::memoryapi::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    };
    use winapi::um::synchapi::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    use winapi::um::winbase::{INFINITE, WAIT_OBJECT_0};
    use winapi::um::winnt::{HANDLE, PAGE_READWRITE};

    const MAX_NAME: usize = 1024;

    /// Keeps the published name alive for the duration of the process.
    pub struct NameHolder {
        _mapping: FileMapping,
        _view: MappedView,
    }

    /// A Windows file mapping
    struct FileMapping {
        name: String,
        handle: HANDLE,
        size: usize,
    }

    impl Drop for FileMapping {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl FileMapping {
        /// Create a new or open an existing mapping with the specified name/size
        pub fn create(name: &str, size: usize) -> anyhow::Result<Self> {
            let wide_name = wide_string(&name);

            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null_mut(),
                    PAGE_READWRITE,
                    0,
                    size as _,
                    wide_name.as_ptr(),
                )
            };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating shared memory with name {}", name));
            }
            Ok(Self {
                name: name.to_string(),
                handle,
                size,
            })
        }

        /// Attempt to open an existing mapping
        pub fn open(name: &str, size: usize) -> anyhow::Result<Self> {
            let wide_name = wide_string(&name);

            let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating shared memory with name {}", name));
            }
            Ok(Self {
                name: name.to_string(),
                handle,
                size,
            })
        }

        /// Map the mapping into the process address space
        pub fn map(&self) -> anyhow::Result<MappedView> {
            let buf =
                unsafe { MapViewOfFile(self.handle, FILE_MAP_ALL_ACCESS, 0, 0, self.size as _) };
            if buf.is_null() {
                return Err(IoError::last_os_error()).with_context(|| {
                    format!("mapping view of shared memory with name {}", self.name)
                });
            }
            Ok(MappedView {
                buf: buf as _,
                size: self.size,
            })
        }
    }

    /// A mutex that can be used to coordinate between processes
    struct NamedMutex {
        handle: HANDLE,
    }
    impl Drop for NamedMutex {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    impl NamedMutex {
        /// Create a mutex with the specified name
        pub fn new(name: &str) -> anyhow::Result<Self> {
            let wide_name = wide_string(name);
            let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide_name.as_ptr()) };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating mutex name {}", name));
            }
            Ok(Self { handle })
        }

        /// Acquire the mutex, and perform `func` while the mutex is held.
        /// Once `func` returns, the mutex is released.
        /// Returns the result of `func`.
        pub fn with_lock<F, T>(&self, func: F) -> anyhow::Result<T>
        where
            F: FnOnce() -> anyhow::Result<T>,
        {
            let res = unsafe { WaitForSingleObject(self.handle, INFINITE) };
            if res != WAIT_OBJECT_0 {
                return Err(IoError::last_os_error()).context("acquire mutex");
            }

            let res = func();
            unsafe { ReleaseMutex(self.handle) };
            res
        }
    }

    /// A materialized view of a mapping
    struct MappedView {
        buf: *mut u8,
        size: usize,
    }

    impl Drop for MappedView {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.buf as _);
            }
        }
    }

    impl MappedView {
        fn slice_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.buf, self.size) }
        }

        fn slice(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.buf, self.size) }
        }
    }

    impl NameHolder {
        /// Computes the names of the objects; they use Local scoped
        /// names so that we have one per desktop, rather than one
        /// system wide.
        fn compute_names(class_name: &str) -> (String, String) {
            let mutex_name = format!("Local\\wezterm-sock-mutex-{}", class_name);
            let map_name = format!("Local\\wezterm-sock-{}", class_name);
            (mutex_name, map_name)
        }

        /// Publish path as the path for class_name.
        pub fn new(path: &Path, class_name: &str) -> anyhow::Result<Self> {
            let (mutex_name, map_name) = Self::compute_names(class_name);
            let path = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("path has no file_name!?"))?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("path is not UTF8!"))?
                .to_string();

            let mutex = NamedMutex::new(&mutex_name)?;
            mutex.with_lock(|| {
                let mapping = FileMapping::create(&map_name, MAX_NAME)?;
                let mut view = mapping.map()?;

                let target_slice = view.slice_mut();
                let len = path.len();

                target_slice[0..len].copy_from_slice(path.as_bytes());
                target_slice[len] = 0;

                log::debug!("published gui path as {}", path);

                Ok(Self {
                    _mapping: mapping,
                    _view: view,
                })
            })
        }

        /// Resolve the existing path for class_name
        pub fn resolve(class_name: &str) -> anyhow::Result<PathBuf> {
            let (mutex_name, map_name) = Self::compute_names(class_name);
            let mutex = NamedMutex::new(&mutex_name)?;
            mutex.with_lock(|| {
                let mapping = FileMapping::open(&map_name, MAX_NAME)?;
                let view = mapping.map()?;

                let source_slice = view.slice();
                let len = source_slice
                    .iter()
                    .position(|&c| c == 0)
                    .ok_or_else(|| anyhow::anyhow!("shared memory is not NUL terminated!"))?;

                let path = std::str::from_utf8(&source_slice[0..len])
                    .context("reading path from shared memory")?;

                let path: PathBuf = path.into();
                // `new` publishes only the file name, so resolve it against
                // the runtime dir rather than the caller's current directory.
                let path = if path.is_absolute() {
                    path
                } else {
                    config::RUNTIME_DIR.join(path)
                };

                Ok(path)
            })
        }
    }

    /// Convert a rust string to a windows wide string
    fn wide_string(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(unix)]
mod unix {
    use super::*;

    pub struct NameHolder {
        published: PathBuf,
        name: PathBuf,
    }

    impl Drop for NameHolder {
        fn drop(&mut self) {
            // If it still points to us, remove the symlink
            if let Ok(target) = std::fs::read_link(&self.name) {
                if target == self.published {
                    log::trace!("removing {}", self.name.display());
                    std::fs::remove_file(&self.name).ok();
                }
            }
        }
    }

    impl NameHolder {
        fn compute_name(class_name: &str) -> String {
            #[cfg(not(target_os = "macos"))]
            {
                let config = config::configuration();
                if config.enable_wayland {
                    if let Ok(wayland) = std::env::var("WAYLAND_DISPLAY") {
                        return format!("wayland-{}-{}", wayland, class_name);
                    }
                    // We don't assume a default WAYLAND_DISPLAY here because
                    // we don't know if the default should be used or if we
                    // should fall back to X11 without connecting to wayland.
                    // We cannot introduce a dep on a wayland client library
                    // here, but we could potentially try to construct a
                    // unix domain socket client to see if our assumed default
                    // is a working unix socket.
                    // Something to fill in later as/when that question arises!
                }
                let x11 = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
                return format!("x11-{}-{}", x11, class_name);
            }
            #[cfg(target_os = "macos")]
            {
                format!("default-{}", class_name)
            }
        }

        fn compute_path(class_name: &str) -> PathBuf {
            config::RUNTIME_DIR.join(Self::compute_name(class_name))
        }

        pub fn new(path: &Path, class_name: &str) -> anyhow::Result<Self> {
            let name = Self::compute_path(class_name);
            std::fs::remove_file(&name).ok();
            std::os::unix::fs::symlink(path, &name)
                .with_context(|| format!("pointing {} -> {}", name.display(), path.display()))?;
            Ok(Self {
                published: path.to_path_buf(),
                name,
            })
        }

        pub fn resolve(class_name: &str) -> anyhow::Result<PathBuf> {
            let name = Self::compute_path(class_name);
            std::fs::read_link(&name).with_context(|| format!("reading symlink {}", name.display()))
        }
    }
}

#[cfg(windows)]
pub use self::windows::NameHolder;

#[cfg(unix)]
pub use self::unix::NameHolder;

/// Unconditionally update the published path to match the provided path,
/// even if there is a running instance with a legitimate published path.
pub fn publish_gui_sock_path(path: &Path, class_name: &str) -> anyhow::Result<NameHolder> {
    NameHolder::new(path, class_name)
}

/// Resolve the last published path for `class_name`.
/// If successful, there is NO guarantee that the returned path references
/// a running instance; it is just the last published path.
pub fn resolve_gui_sock_path(class_name: &str) -> anyhow::Result<PathBuf> {
    NameHolder::resolve(class_name)
}

/// This function returns a list of the gui-sock- paths in
/// the runtime dir.  These represent the locally running
/// instances of wezterm-gui.
/// The list is pruned of any entries that are not live
/// and then sorted with the eldest instance first.
pub fn discover_gui_socks() -> Vec<PathBuf> {
    let mut socks = vec![];

    #[derive(Debug)]
    struct Entry {
        path: PathBuf,
        age: Duration,
    }

    /// Get an idea of the age of the entry.
    /// Some filesystems don't support reporting `created`,
    /// so fall back on `modified`.
    fn meta_age(meta: &std::fs::Metadata) -> Duration {
        let t = if let Ok(created) = meta.created() {
            created
        } else if let Ok(changed) = meta.modified() {
            changed
        } else {
            return Duration::from_millis(300);
        };
        if let Ok(d) = SystemTime::now().duration_since(t) {
            d
        } else {
            Duration::from_millis(300)
        }
    }

    if let Ok(dir) = std::fs::read_dir(&*config::RUNTIME_DIR) {
        for entry in dir {
            if let Ok(entry) = entry {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("gui-sock-") {
                        let path = entry.path();
                        if let Ok(meta) = entry.metadata() {
                            let age = meta_age(&meta);
                            if is_sock_dead(&path) && age > Duration::from_secs(1) {
                                let _ = std::fs::remove_file(&path);
                            } else {
                                socks.push(Entry { path, age });
                            }
                        }
                    }
                }
            }
        }
    }

    socks.sort_by(|a, b| a.age.cmp(&b.age).reverse());
    log::trace!("{:?}", socks);
    socks.into_iter().map(|e| e.path).collect()
}

fn is_sock_dead(sock: &std::path::Path) -> bool {
    UnixStream::connect(sock).is_err()
}

/// Remove `agent.*` symlinks whose target no longer exists.  SSH agent
/// forwarding creates one symlink per pane; panes that go away without a clean
/// shutdown leave the link behind and nothing else prunes them.  Only dangling
/// symlinks are removed: live agent sockets and regular files are untouched.
pub fn prune_dead_agent_links() -> usize {
    prune_dead_agent_links_in(&config::RUNTIME_DIR)
}

fn prune_dead_agent_links_in(dir: &std::path::Path) -> usize {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with("agent.") {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        match std::fs::metadata(&path) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
            Err(_) => {}
        }
    }
    removed
}

/// Age after which an orphaned GUI log is pruned.
const GUI_LOG_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Extract the pid from `wezterm*-log-<pid>.txt`.
fn gui_log_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("wezterm")?;
    let rest = rest.strip_suffix(".txt")?;
    let idx = rest.find("-log-")?;
    let pid = &rest[idx + 5..];
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// Whether a pid is alive. `None` means the check is unavailable (never
/// delete on an unknown answer).
fn pid_is_alive(pid: u32) -> Option<bool> {
    #[cfg(windows)]
    unsafe {
        use winapi::shared::minwindef::FALSE;
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if handle.is_null() {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(87) => Some(false), // ERROR_INVALID_PARAMETER: no such process
                Some(5) => Some(true),   // ERROR_ACCESS_DENIED: exists, protected
                _ => None,
            };
        }
        CloseHandle(handle);
        Some(true)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// Remove `wezterm*-log-<pid>.txt` files whose process is gone and that are
/// older than a week. The current process's own log is never touched.
pub fn prune_old_gui_logs() -> usize {
    prune_old_gui_logs_in(
        &config::RUNTIME_DIR,
        std::process::id(),
        SystemTime::now(),
        &pid_is_alive,
    )
}

fn prune_old_gui_logs_in(
    dir: &std::path::Path,
    current_pid: u32,
    now: SystemTime,
    alive: &dyn Fn(u32) -> Option<bool>,
) -> usize {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(pid) = gui_log_pid(name) else {
            continue;
        };
        if pid == current_pid {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let old = now
            .duration_since(modified)
            .map(|age| age > GUI_LOG_MAX_AGE)
            .unwrap_or(false);
        if !old {
            continue;
        }
        if alive(pid) == Some(false) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_log_pid_parses_only_wezterm_logs() {
        assert_eq!(gui_log_pid("wezterm-gui.exe-log-123.txt"), Some(123));
        assert_eq!(gui_log_pid("wezterm.exe-log-7.txt"), Some(7));
        assert_eq!(gui_log_pid("wezterm-gui.exe-log-.txt"), None);
        assert_eq!(gui_log_pid("wezterm-gui.exe-log-abc.txt"), None);
        assert_eq!(gui_log_pid("other-log-1.txt"), None);
        assert_eq!(gui_log_pid("wezterm-gui.exe-log-1.log"), None);
    }

    #[test]
    fn prunes_only_old_dead_logs() {
        let dir = std::env::temp_dir().join(format!(
            "wezterm-log-prune-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let old = now - Duration::from_secs(8 * 24 * 60 * 60);
        let make = |name: &str, mtime: SystemTime| {
            let path = dir.join(name);
            std::fs::write(&path, b"log").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(mtime))
                .unwrap();
            path
        };
        let dead_old = make("wezterm-gui.exe-log-111.txt", old);
        let live_old = make("wezterm.exe-log-222.txt", old);
        let dead_new = make("wezterm-gui.exe-log-333.txt", now);
        let current = make(&format!("wezterm-gui.exe-log-{}.txt", std::process::id()), old);
        let other = dir.join("notes.txt");
        std::fs::write(&other, b"x").unwrap();

        let removed = prune_old_gui_logs_in(&dir, std::process::id(), now, &|pid| match pid {
            111 => Some(false),
            222 => Some(true),
            333 => Some(false),
            _ => Some(false),
        });
        assert_eq!(removed, 1);
        assert!(!dead_old.exists());
        assert!(live_old.exists());
        assert!(dead_new.exists());
        assert!(current.exists());
        assert!(other.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
