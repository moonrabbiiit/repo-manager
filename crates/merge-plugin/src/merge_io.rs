//! 文件提交时保留被实际替换/删除的版本，防止“核对之后、提交之前”的原子保存吞掉新编辑。
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn private_path(path: &Path, kind: &str) -> Result<PathBuf, String> {
    let parent = path.parent().ok_or("文件没有父目录")?;
    Ok(parent.join(format!(".repomanager-merge-recovery-{kind}-{}-{}-{}",
        std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed))))
}

fn matches(path: &Path, expected: &[u8]) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut buffer = [0u8; 64 * 1024];
    let mut offset = 0usize;
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 { return Ok(offset == expected.len()) }
        if expected.get(offset..offset.saturating_add(count)) != Some(&buffer[..count]) { return Ok(false) }
        offset += count;
    }
}

fn freeze(path: &Path) -> Result<File, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(5);
    }
    options.open(path).map_err(|e| format!("文件仍有写操作：{e}"))
}

#[cfg(windows)]
fn wide(path: &Path) -> Result<Vec<u16>, String> {
    use std::os::windows::ffi::OsStrExt;
    let mut name: Vec<u16> = std::path::absolute(path).map_err(|e| e.to_string())?.as_os_str().encode_wide().collect();
    if name.contains(&0) { return Err("文件路径包含 NUL".into()) }
    name.push(0);
    Ok(name)
}

#[cfg(windows)]
fn move_new(source: &Path, dest: &Path) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let source = wide(source)?;
    let dest = wide(dest)?;
    if unsafe { MoveFileExW(source.as_ptr(), dest.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn move_new(source: &Path, dest: &Path) -> Result<(), String> {
    fs::hard_link(source, dest).map_err(|e| e.to_string())?;
    fs::remove_file(source).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn replace_capture(dest: &Path, source: &Path, backup: &Path) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let (dest, source, backup) = (wide(dest)?, wide(source)?, wide(backup)?);
    if unsafe { ReplaceFileW(dest.as_ptr(), source.as_ptr(), backup.as_ptr(), 0,
        std::ptr::null(), std::ptr::null()) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_capture(dest: &Path, source: &Path, backup: &Path) -> Result<(), String> {
    move_new(dest, backup)?;
    if let Err(error) = move_new(source, dest) {
        let _ = move_new(backup, dest);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static BEFORE_RECOVERY: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> = const { std::cell::RefCell::new(None) };
}

fn commit_hook(_path: &Path) {
    #[cfg(test)]
    BEFORE_COMMIT.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() { callback(_path); }
    });
}

/// 调用方已核对来源；这里再次核对操作实际取走的目标，并保存所有不确定版本。
pub fn change(path: &Path, expected: Option<&[u8]>, replacement: Option<&[u8]>) -> Result<(), String> {
    if fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) {
        return Err(format!("目标是只读文件：{}", path.display()));
    }
    if expected.is_none() && replacement.is_none() { return Ok(()) }
    let prepared = if let Some(bytes) = replacement {
        fs::create_dir_all(path.parent().ok_or("目标没有父目录")?).map_err(|e| e.to_string())?;
        let temp = private_path(path, "prepared")?;
        let mut file = fs::OpenOptions::new().create_new(true).write(true).open(&temp).map_err(|e| e.to_string())?;
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(error.to_string());
        }
        Some(temp)
    } else { None };
    commit_hook(path);
    let Some(expected) = expected else {
        let source = prepared.as_ref().ok_or("缺少待安装文件")?;
        let result = move_new(source, path);
        if result.is_err() { let _ = fs::remove_file(source); }
        return result.map_err(|e| format!("新增目标已存在或无法安装，未覆盖：{e}"));
    };
    let captured = private_path(path, "captured")?;
    let installed = if let Some(source) = &prepared { replace_capture(path, source, &captured) }
        else { move_new(path, &captured) };
    if let Err(error) = installed {
        // ReplaceFileW 报错时也可能已移动部分文件；绝不删除不确定的恢复材料。
        return Err(format!("提交失败：{error}；请保留恢复文件 {} 和 {:?}", captured.display(), prepared));
    }
    let _captured_lease = freeze(&captured).map_err(|e| format!("{e}；原文保留在 {}，停止合并", captured.display()))?;
    let same = matches(&captured, expected).map_err(|e| format!("无法核对实际原文：{e}；保留 {}", captured.display()))?;
    if same {
        fs::remove_file(&captured).map_err(|e| format!("已提交，但恢复临时文件未清理：{}：{e}", captured.display()))?;
        return Ok(());
    }
    drop(_captured_lease); // ReplaceFileW 需要为替换源取得写权限；私有恢复路径不再暴露给编辑器。
    #[cfg(test)]
    BEFORE_RECOVERY.with(|hook| { if let Some(callback) = hook.borrow_mut().take() { callback(path); } });
    // 外部原子保存先于我们提交。先把它取回；第二次竞争产生的版本也必须留下。
    if let Some(ours) = replacement {
        let intervening = private_path(path, "intervening")?;
        match replace_capture(path, &captured, &intervening) {
            Ok(()) => {
                let _lease = freeze(&intervening).map_err(|e| format!("{e}；并发版本保留在 {}", intervening.display()))?;
                if matches(&intervening, ours)? {
                    let _ = fs::remove_file(&intervening);
                    Err("提交时检测到外部原子保存，已恢复外部编辑，取消本文件合并".into())
                } else {
                    Err(format!("恢复期间再次发生并发保存，停止合并；另一份新编辑保留在 {}", intervening.display()))
                }
            }
            Err(error) => Err(format!("检测到并发保存，无法自动恢复（{error}）；新编辑保留在 {} 与 {}", captured.display(), intervening.display())),
        }
    } else {
        match move_new(&captured, path) {
            Ok(()) => Err("删除时检测到外部原子保存，已恢复外部编辑，取消删除".into()),
            Err(error) => Err(format!("删除时发生并发保存（{error}），未覆盖新目标；原文保留在 {}", captured.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tree(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("repo-manager-cas-{label}-{}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&root).unwrap();
        root.join("file.txt")
    }
    #[test]
    fn normal_replace_and_delete_work() {
        let path = tree("normal");
        fs::write(&path, b"old").unwrap();
        let _lease = freeze(&path).unwrap();
        change(&path, Some(b"old"), Some(b"merged")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"merged");
        change(&path, Some(b"merged"), None).unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn an_atomic_editor_save_between_check_and_replace_is_restored() {
        for deleting in [false, true] {
            let path = tree(if deleting { "delete-race" } else { "replace-race" });
            fs::write(&path, b"old").unwrap();
            BEFORE_COMMIT.with(|hook| *hook.borrow_mut() = Some(Box::new(|path| {
                let temp = path.with_extension("editor");
                fs::write(&temp, b"new edit").unwrap();
                fs::rename(temp, path).unwrap();
            })));
            let _lease = freeze(&path).unwrap();
            let error = change(&path, Some(b"old"), if deleting { None } else { Some(b"merged") }).unwrap_err();
            assert!(error.contains("外部原子保存"), "{error}");
            assert_eq!(fs::read(&path).unwrap(), b"new edit");
        }
    }
    #[test]
    fn a_second_atomic_save_during_recovery_keeps_both_edits() {
        let path = tree("recovery-race");
        fs::write(&path, b"old").unwrap();
        let save = |bytes: &'static [u8]| -> Box<dyn FnOnce(&Path)> { Box::new(move |path| {
            let temp = path.with_extension("editor");
            fs::write(&temp, bytes).unwrap(); fs::rename(temp, path).unwrap();
        }) };
        BEFORE_COMMIT.with(|h| *h.borrow_mut() = Some(save(b"first edit")));
        BEFORE_RECOVERY.with(|h| *h.borrow_mut() = Some(save(b"second edit")));
        let error = change(&path, Some(b"old"), Some(b"merged")).unwrap_err();
        assert!(error.contains("再次发生"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"first edit");
        let preserved = fs::read_dir(path.parent().unwrap()).unwrap().filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("intervening"))
            .map(|e| fs::read(e.path()).unwrap()).collect::<Vec<_>>();
        assert_eq!(preserved, vec![b"second edit".to_vec()]);
    }
    #[test]
    fn new_targets_are_never_overwritten() {
        let path = tree("fresh-race");
        BEFORE_COMMIT.with(|hook| *hook.borrow_mut() = Some(Box::new(|path| fs::write(path, b"someone else").unwrap())));
        assert!(change(&path, None, Some(b"ours")).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"someone else");
    }
}
