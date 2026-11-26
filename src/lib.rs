use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundDir {
    pub path: PathBuf,
    pub size_bytes: u64,
}

impl FoundDir {
    pub fn project_name(&self) -> String {
        self.path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn relative_project_path(&self, root: &Path) -> String {
        if let Some(parent) = self.path.parent() {
            if let Ok(rel) = parent.strip_prefix(root) {
                let s = rel.display().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
        self.project_name()
    }
}

pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn should_skip_dir_name(name: &std::ffi::OsStr) -> bool {
    let Some(s) = name.to_str() else {
        return false;
    };
    matches!(
        s.to_ascii_lowercase().as_str(),
        ".git"
            | "target"
            | ".cargo"
            | ".rustup"
            | ".venv"
            | "venv"
            | "__pycache__"
            | "appdata"
            | "$recycle.bin"
            | "system volume information"
            | "windows"
            | "program files"
            | "program files (x86)"
            | "programdata"
    )
}

pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_symlink() {
                continue;
            }

            if file_type.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            } else if file_type.is_dir() {
                stack.push(entry.path());
            }
        }
    }

    total
}

struct WorkQueue {
    dirs: Mutex<VecDeque<PathBuf>>,
    active_workers: AtomicUsize,
    done: AtomicBool,
    cv: Condvar,
}

impl WorkQueue {
    fn new(root: PathBuf) -> Self {
        let mut dirs = VecDeque::new();
        dirs.push_back(root);
        Self {
            dirs: Mutex::new(dirs),
            active_workers: AtomicUsize::new(0),
            done: AtomicBool::new(false),
            cv: Condvar::new(),
        }
    }

    fn push_many(&self, new_dirs: Vec<PathBuf>) {
        if new_dirs.is_empty() {
            return;
        }
        let mut guard = self.dirs.lock().unwrap();
        guard.extend(new_dirs);
        self.cv.notify_all();
    }

    fn pop(&self) -> Option<PathBuf> {
        let mut guard = self.dirs.lock().unwrap();
        loop {
            if let Some(dir) = guard.pop_front() {
                self.active_workers.fetch_add(1, Ordering::SeqCst);
                return Some(dir);
            }
            if self.done.load(Ordering::SeqCst) {
                return None;
            }
            if self.active_workers.load(Ordering::SeqCst) == 0 {
                self.done.store(true, Ordering::SeqCst);
                self.cv.notify_all();
                return None;
            }
            guard = self.cv.wait(guard).unwrap();
        }
    }

    fn finish_one(&self) {
        let prev = self.active_workers.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            let guard = self.dirs.lock().unwrap();
            if guard.is_empty() {
                self.done.store(true, Ordering::SeqCst);
                self.cv.notify_all();
            }
        }
    }
}


pub fn scan_node_modules(root: &Path, min_bytes: u64) -> io::Result<Vec<FoundDir>> {
    scan_node_modules_with_progress(root, min_bytes, None)
}

pub fn scan_node_modules_with_progress(
    root: &Path,
    min_bytes: u64,
    found_counter: Option<&AtomicUsize>,
) -> io::Result<Vec<FoundDir>> {
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Directory not found: {}", root.display()),
        ));
    }

    let queue = WorkQueue::new(root.to_path_buf());
    let nm_paths = Mutex::new(Vec::<PathBuf>::new());
    let workers = thread::available_parallelism()
        .map(|n| n.get().clamp(2, 16))
        .unwrap_or(4);

    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                while let Some(dir) = queue.pop() {
                    let mut subdirs = Vec::new();
                    let mut local_nm = Vec::new();

                    if let Ok(entries) = fs::read_dir(&dir) {
                        for entry in entries.flatten() {
                            let Ok(file_type) = entry.file_type() else {
                                continue;
                            };
                            if !file_type.is_dir() || file_type.is_symlink() {
                                continue;
                            }

                            let name = entry.file_name();
                            if name == "node_modules" {
                                local_nm.push(entry.path());
                                if let Some(counter) = found_counter {
                                    counter.fetch_add(1, Ordering::Relaxed);
                                }
                                continue;
                            }

                            if should_skip_dir_name(&name) {
                                continue;
                            }

                            subdirs.push(entry.path());
                        }
                    }

                    if !local_nm.is_empty() {
                        nm_paths.lock().unwrap().extend(local_nm);
                    }
                    queue.push_many(subdirs);
                    queue.finish_one();
                }
            });
        }
    });

    let paths = nm_paths.into_inner().unwrap();
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let next_idx = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(paths.len()));
    let size_workers = workers.min(paths.len());

    thread::scope(|s| {
        for _ in 0..size_workers {
            s.spawn(|| {
                let mut local_results = Vec::new();
                loop {
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                    if idx >= paths.len() {
                        break;
                    }
                    let path = &paths[idx];
                    let size_bytes = dir_size(path);
                    if size_bytes >= min_bytes {
                        local_results.push(FoundDir {
                            path: path.clone(),
                            size_bytes,
                        });
                    }
                }
                if !local_results.is_empty() {
                    results.lock().unwrap().extend(local_results);
                }
            });
        }
    });

    let mut found = results.into_inner().unwrap();
    found.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    Ok(found)
}

pub fn delete_dir(target: &FoundDir) -> io::Result<u64> {
    fs::remove_dir_all(&target.path)?;
    Ok(target.size_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nm_cleaner_test_{label}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn formats_sizes_correctly() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.00 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn scans_without_descending_into_nested_node_modules_or_git() {
        let root = make_temp_dir("scan");

        let proj_a = root.join("app-a");
        let nm_a = proj_a.join("node_modules");
        let nested_nm = nm_a.join("pkg").join("node_modules");
        fs::create_dir_all(&nested_nm).unwrap();
        fs::write(nm_a.join("index.js"), vec![0u8; 100]).unwrap();
        fs::write(nested_nm.join("sub.js"), vec![0u8; 50]).unwrap();

        let proj_b = root.join("app-b");
        let nm_b = proj_b.join("node_modules");
        fs::create_dir_all(&nm_b).unwrap();
        fs::write(nm_b.join("big.bin"), vec![0u8; 300]).unwrap();

        let git_nm = root.join(".git").join("node_modules");
        fs::create_dir_all(&git_nm).unwrap();
        fs::write(git_nm.join("ignored.js"), vec![0u8; 999]).unwrap();

        let found = scan_node_modules(&root, 0).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, nm_b);
        assert_eq!(found[0].size_bytes, 300);
        assert_eq!(found[1].path, nm_a);
        assert_eq!(found[1].size_bytes, 150);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn extracts_project_name_from_parent_dir() {
        let root = Path::new("projects");
        let item = FoundDir {
            path: root.join("work").join("my-app").join("node_modules"),
            size_bytes: 1024,
        };
        assert_eq!(item.project_name(), "my-app");
        assert_eq!(
            item.relative_project_path(root),
            PathBuf::from("work").join("my-app").display().to_string()
        );
    }

    #[test]
    fn deletes_only_selected_node_modules() {
        let root = make_temp_dir("delete");
        let proj = root.join("my-app");
        let nm = proj.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("dep.js"), b"hello").unwrap();
        fs::write(proj.join("package.json"), b"{}").unwrap();

        let found = scan_node_modules(&root, 0).unwrap();
        assert_eq!(found.len(), 1);

        let freed = delete_dir(&found[0]).unwrap();
        assert_eq!(freed, 5);
        assert!(!nm.exists());
        assert!(proj.join("package.json").exists());

        fs::remove_dir_all(&root).unwrap();
    }
}
