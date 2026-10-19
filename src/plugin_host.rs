//! 插件宿主：从 `plugin\*.dll` 加载，按 C ABI 调用。
//!
//! 插件是真正的动态库——文件不在，相关功能就不存在（不是界面上藏起来）。
//! 卸载 = 删文件；装载 = 把 dll 放回目录（下次调用时自动加载）。
//!
//! 每个插件只暴露三个符号，数据一律走 JSON 文本：
//! `rm_plugin_abi()` / `rm_plugin_call(op, json)` / `rm_plugin_free(ptr)`。
//! 这样跨 ABI 的只有标量与裸指针，Rust 的结构体布局不参与其中。

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

/// ABI 版本，必须和插件里的 `rm_plugin_abi()` 一致
pub const ABI: u32 = 1;

/// 插件目录：exe 同级的 `plugin\`（老的 `plugin\` 也认，便携版 / 升级后都不断档）。
/// `REPO_MANAGER_PLUGIN_DIR` 可覆盖（测试用）。
///
/// 开发期 exe 在 `target\release\`，插件放在仓库根的 `plugin\`：往上找几层，
/// 命中带 dll 的那个 `plugin\` 就用它，免得每改一次插件都要手动往 target 里拷。
pub fn plugin_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("REPO_MANAGER_PLUGIN_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    // 先看 exe 旁边：新名字优先，老目录兜底
    for name in ["plugin", "plugins"] {
        let dir = exe_dir.join(name);
        if dir.is_dir() {
            return dir;
        }
    }
    // 再往上找（target\release → target → 仓库根）
    let mut cur = exe_dir.as_path();
    for _ in 0..4 {
        let Some(parent) = cur.parent() else { break };
        let dir = parent.join("plugin");
        if dir.join(crate::plugins::MERGE_DLL).is_file()
            || dir.join(crate::plugins::AI_LOG_DLL).is_file()
        {
            return dir;
        }
        cur = parent;
    }
    exe_dir.join("plugin")
}

/// 已加载的库，按 dll 文件名索引。卸载 / 重装之后要 `forget_one()`，
/// 否则 Windows 下文件被映射着，删不掉也覆盖不了。
static LIBS: Mutex<Option<HashMap<&'static str, libloading::Library>>> = Mutex::new(None);

/// 丢掉全部已加载的句柄
pub fn forget() {
    let mut guard = LIBS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// 丢掉某一个插件的句柄（卸载它之前必须调）
pub fn forget_one(dll: &str) {
    let mut guard = LIBS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(dll);
    }
}

/// 插件文件在不在
pub fn present(dll: &str) -> bool {
    plugin_dir().join(dll).is_file()
}

fn with_lib<T>(
    dll: &'static str,
    f: impl FnOnce(&libloading::Library) -> Result<T, String>,
) -> Result<T, String> {
    let mut guard = LIBS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if !map.contains_key(dll) {
        let path = plugin_dir().join(dll);
        if !path.is_file() {
            return Err(format!("插件未安装：{}", path.display()));
        }
        let lib = unsafe { libloading::Library::new(&path) }
            .map_err(|e| format!("加载插件失败（{}）：{e}", path.display()))?;
        // 版本校验：ABI 对不上就拒绝，别让不匹配的 dll 把主程序带崩
        let abi = unsafe {
            lib.get::<unsafe extern "C" fn() -> u32>(b"rm_plugin_abi\0")
                .map_err(|e| format!("不是插件（缺 rm_plugin_abi）：{e}"))?
                .clone()
        };
        let got = unsafe { abi() };
        if got != ABI {
            return Err(format!("插件 ABI 版本不匹配：插件 {got}，主程序 {ABI}"));
        }
        map.insert(dll, lib);
    }
    f(map.get(dll).expect("刚刚装进去的库"))
}

/// 调一次插件。`op` 是操作名，`payload` 是 JSON 参数；回包解出 `data` 字段。
pub fn call(dll: &'static str, op: &str, payload: Value) -> Result<Value, String> {
    let reply = with_lib(dll, |lib| unsafe {
        let call = lib
            .get::<unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char>(
                b"rm_plugin_call\0",
            )
            .map_err(|e| format!("插件缺少 rm_plugin_call：{e}"))?
            .clone();
        let free = lib
            .get::<unsafe extern "C" fn(*mut c_char)>(b"rm_plugin_free\0")
            .map_err(|e| format!("插件缺少 rm_plugin_free：{e}"))?
            .clone();
        let op = CString::new(op).map_err(|e| e.to_string())?;
        let body = CString::new(payload.to_string()).map_err(|e| e.to_string())?;
        let ptr = call(op.as_ptr(), body.as_ptr());
        if ptr.is_null() {
            return Err("插件返回了空指针".to_owned());
        }
        let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        free(ptr);
        Ok(text)
    })?;

    parse_reply(&reply)
}

/// 拆插件回的那层 `{ok, data | error}` 信封
fn parse_reply(reply: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(reply).map_err(|e| format!("插件回包不是 JSON：{e}（{reply}）"))?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("插件报错但没给原因")
            .to_owned())
    }
}

/// 插件自报的身份（用于面板显示与导入校验）
pub fn info(dll: &'static str) -> Result<Value, String> {
    call(dll, "info", Value::Null)
}

/// 问**任意路径**上那个 dll 的身份：导入时文件还没进插件目录，先就地验一遍。
/// 用完立刻把库卸掉，不留句柄（Windows 下留着就删不了源文件）。
pub fn info_at(path: &Path) -> Result<Value, String> {
    let lib = unsafe { libloading::Library::new(path) }
        .map_err(|e| format!("加载插件失败（{}）：{e}", path.display()))?;
    // 版本校验：ABI 对不上就拒绝，别让不匹配的 dll 把主程序带崩
    let abi = unsafe {
        lib.get::<unsafe extern "C" fn() -> u32>(b"rm_plugin_abi\0")
            .map_err(|e| format!("不是插件（缺 rm_plugin_abi）：{e}"))?
            .clone()
    };
    let got = unsafe { abi() };
    if got != ABI {
        return Err(format!("插件 ABI 版本不匹配：插件 {got}，主程序 {ABI}"));
    }
    let reply = {
        let call = unsafe {
            lib.get::<unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char>(
                b"rm_plugin_call\0",
            )
            .map_err(|e| format!("不是插件（缺 rm_plugin_call）：{e}"))?
            .clone()
        };
        let free = unsafe {
            lib.get::<unsafe extern "C" fn(*mut c_char)>(b"rm_plugin_free\0")
                .map_err(|e| format!("不是插件（缺 rm_plugin_free）：{e}"))?
                .clone()
        };
        let op = CString::new("info").map_err(|e| e.to_string())?;
        let body = CString::new("null").map_err(|e| e.to_string())?;
        let ptr = unsafe { call(op.as_ptr(), body.as_ptr()) };
        if ptr.is_null() {
            return Err("插件返回了空指针".to_owned());
        }
        let text = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
        unsafe { free(ptr) };
        text
    };
    parse_reply(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真的把 dll 加载起来问一次身份。平时（没设插件目录 / 没构建插件）自动跳过，
    /// 需要时用 REPO_MANAGER_PLUGIN_DIR 指到 target\release\plugins 手动验一遍
    #[test]
    fn the_merge_plugin_answers_over_the_abi() {
        let Ok(dir) = std::env::var("REPO_MANAGER_PLUGIN_DIR") else {
            return;
        };
        if !std::path::Path::new(&dir).join(crate::plugins::MERGE_DLL).is_file() {
            return;
        }
        let info = info(crate::plugins::MERGE_DLL).expect("插件应能自报身份");
        assert_eq!(info.get("id").and_then(|v| v.as_str()), Some("merge"));
        assert_eq!(info.get("abi").and_then(|v| v.as_u64()), Some(ABI as u64));
        assert_eq!(info.get("name").and_then(|v| v.as_str()), Some("双向合并"));
    }

    /// 开发期 exe 在 `target\debug\deps\` 下，插件却在仓库根的 `plugin\`：
    /// 目录解析要往上走，而不是盯着 exe 旁边那个空位置
    #[test]
    fn the_plugin_dir_walks_up_to_the_repo_root() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::remove_var("REPO_MANAGER_PLUGIN_DIR");
        let dir = plugin_dir();
        assert_eq!(
            dir.file_name().and_then(|s| s.to_str()),
            Some("plugin"),
            "插件目录应叫 plugin：{}",
            dir.display()
        );
        if dir.join(crate::plugins::MERGE_DLL).is_file() {
            let exe = std::env::current_exe().expect("测试进程也有 exe 路径");
            let repo = dir.parent().expect("plugin 目录总有上级");
            assert!(
                exe.starts_with(repo),
                "找到的应该是含 exe 的仓库里的 plugin（{}），实际 exe 在 {}",
                dir.display(),
                exe.display()
            );
        }
    }

    /// 同一个 ABIs、同一套加载路径，AI 日志插件也要能自报身份
    #[test]
    fn the_ai_log_plugin_answers_over_the_abi() {
        let Ok(dir) = std::env::var("REPO_MANAGER_PLUGIN_DIR") else {
            return;
        };
        if !std::path::Path::new(&dir).join(crate::plugins::AI_LOG_DLL).is_file() {
            return;
        }
        let info = info(crate::plugins::AI_LOG_DLL).expect("插件应能自报身份");
        assert_eq!(info.get("id").and_then(|v| v.as_str()), Some("ai-log"));
        assert_eq!(info.get("abi").and_then(|v| v.as_u64()), Some(ABI as u64));
        assert_eq!(info.get("name").and_then(|v| v.as_str()), Some("AI 工作日志"));
    }
}
