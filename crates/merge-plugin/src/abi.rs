//! 插件的 C ABI：一个 `abi` 查询 + 一个 JSON RPC 入口。
//!
//! 主程序用 `libloading` 按名取符号，调用完用 `rm_plugin_free` 还内存。
//! 请求/回包都是 UTF-8 JSON；回包固定 `{"ok":true,"data":...}` 或 `{"ok":false,"error":"..."}`。

use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::merge;
use crate::svn;

/// ABI 版本：主程序与插件必须一致，否则主程序拒绝加载
pub const ABI_VERSION: u32 = 1;

#[no_mangle]
pub extern "C" fn rm_plugin_abi() -> u32 {
    ABI_VERSION
}

/// 唯一的调用入口。`op` = 操作名，`payload` = 该操作的 JSON 参数（可为空字符串）。
/// 返回堆上的 C 字符串，调用方用完必须交给 `rm_plugin_free`。
#[no_mangle]
pub extern "C" fn rm_plugin_call(op: *const c_char, payload: *const c_char) -> *mut c_char {
    let op = unsafe { text(op) };
    let payload = unsafe { text(payload) };
    // 插件里的 panic 不能跨 FFI 边界炸穿主程序
    let reply = std::panic::catch_unwind(|| dispatch(&op, &payload))
        .unwrap_or_else(|_| err("插件内部崩溃（panic）"));
    CString::new(reply)
        .unwrap_or_else(|_| CString::new(err("回包里有 NUL")).unwrap())
        .into_raw()
}

#[no_mangle]
pub extern "C" fn rm_plugin_free(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(p));
    }
}

unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

fn ok(data: impl serde::Serialize) -> String {
    json!({ "ok": true, "data": data }).to_string()
}

fn err(message: impl AsRef<str>) -> String {
    json!({ "ok": false, "error": message.as_ref() }).to_string()
}

fn dispatch(op: &str, payload: &str) -> String {
    match op {
        "info" => ok(json!({
            "id": "merge",
            "name": "双向合并",
            "version": env!("CARGO_PKG_VERSION"),
            "abi": ABI_VERSION,
        })),
        "plan" => plan(payload),
        "apply" => apply(payload),
        "restore" => restore(payload),
        "backups" => backups(payload),
        "clean" => clean(payload),
        _ => err(format!("不认识的操作：{op}")),
    }
}

// ---------------------------------------------------------------- 各操作的入参

#[derive(Deserialize)]
struct SvnConn {
    #[serde(default)]
    exe: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
}

impl SvnConn {
    fn client(&self) -> Option<svn::Svn> {
        (!self.exe.trim().is_empty()).then(|| svn::Svn {
            exe: PathBuf::from(self.exe.trim()),
            username: self.user.clone(),
            password: self.password.clone(),
        })
    }

    fn required(&self) -> Result<svn::Svn, String> {
        self.client()
            .ok_or_else(|| "svn.exe 路径为空，读不了服务器端".to_owned())
    }
}

#[derive(Deserialize)]
struct PlanRequest {
    #[serde(flatten)]
    svn: SvnConn,
    local_root: String,
    server_root: String,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    include: String,
    #[serde(default)]
    exclude: String,
    #[serde(default)]
    state_path: Option<String>,
    #[serde(default)]
    lcs_cell_limit: usize,
}

#[derive(Deserialize)]
struct ApplyRequest {
    plan_id: u64,
    root: String,
    #[serde(default)]
    backup: bool,
}

/// 预览算出的完整计划留在插件里（含要写的字节），只把「清单视图」发给主程序。
/// 落盘按编号取回，省掉几十 MB 的 JSON 搬运；最多留 4 份，新的一来就淘汰旧的。
static PLANS: Mutex<Option<HashMap<u64, merge::Plan>>> = Mutex::new(None);
static NEXT_PLAN: AtomicU64 = AtomicU64::new(1);

fn stash(plan: merge::Plan) -> u64 {
    let id = NEXT_PLAN.fetch_add(1, Ordering::Relaxed);
    let mut guard = PLANS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= 4 {
        if let Some(oldest) = map.keys().min().copied() {
            map.remove(&oldest);
        }
    }
    map.insert(id, plan);
    id
}

fn take_plan(id: u64) -> Option<merge::Plan> {
    let mut guard = PLANS.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_mut().and_then(|map| map.get(&id).cloned())
}

#[derive(Deserialize)]
struct RestoreRequest {
    #[serde(flatten)]
    svn: SvnConn,
    backup: String,
}

#[derive(Deserialize)]
struct RootRequest {
    root: String,
}

#[derive(Deserialize)]
struct Empty {}

fn parse<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, String> {
    if payload.trim().is_empty() {
        return serde_json::from_str("{}").map_err(|e| format!("参数解析失败：{e}"));
    }
    serde_json::from_str(payload).map_err(|e| format!("参数解析失败：{e}"))
}

// ---------------------------------------------------------------- 各操作

fn plan(payload: &str) -> String {
    let req: PlanRequest = match parse(payload) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let svn = match req.svn.required() {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let local = PathBuf::from(&req.local_root);
    let server = PathBuf::from(&req.server_root);
    let anc = svn::SvnAncestor::new(svn.clone(), &server);
    let local_anc = svn::SvnAncestor::new_local(svn, &local);
    if let Some(reason) = anc.error() {
        return err(format!("读不到工作副本状态，无法合并：{reason}"));
    }
    let limits = merge::MergeLimits {
        lcs_cell_limit: if req.lcs_cell_limit == 0 {
            merge::LCS_CELL_LIMIT
        } else {
            req.lcs_cell_limit
        },
        ..Default::default()
    };
    let state = req.state_path.map(PathBuf::from);
    match merge::plan_with_state(
        &local,
        &server,
        &req.filter,
        &req.include,
        &req.exclude,
        &anc,
        &local_anc,
        &limits,
        state.as_deref(),
    ) {
        Ok(plan) => {
            let view = plan.clone();
            let bytes = plan.backup_bytes();
            let id = stash(plan);
            ok(json!({
                "id": id,
                "backup_bytes": bytes,
                "plan": view,
                "local": local.display().to_string(),
                "server": server.display().to_string(),
            }))
        }
        Err(message) => err(message),
    }
}

fn apply(payload: &str) -> String {
    let req: ApplyRequest = match parse(payload) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let Some(plan) = take_plan(req.plan_id) else {
        return err("插件里没有这份计划（可能已过期），请重新预览");
    };
    match merge::apply(&plan, &PathBuf::from(&req.root), req.backup) {
        Ok(done) => ok(json!({
            "written": done.written,
            "bytes": done.bytes,
            "failed": done.failed,
            "backup": done.backup.map(|d| d.display().to_string()),
        })),
        Err(message) => err(message),
    }
}

fn restore(payload: &str) -> String {
    let req: RestoreRequest = match parse(payload) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let client = req.svn.client();
    match merge::restore_with_client(&PathBuf::from(&req.backup), client.as_ref()) {
        Ok(done) => ok(json!({
            "restored": done.restored,
            "removed": done.removed,
            "failed": done.failed,
        })),
        Err(message) => err(message),
    }
}

fn backups(payload: &str) -> String {
    let req: RootRequest = match parse(payload) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    ok(merge::backups(&PathBuf::from(&req.root)))
}

fn clean(payload: &str) -> String {
    let req: RootRequest = match parse(payload) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let (count, bytes) = merge::clean_backups(&PathBuf::from(&req.root));
    ok(json!({ "count": count, "bytes": bytes }))
}

/// 给主程序一份「这插件能干什么」的静态说明，面板上直接展示
#[allow(dead_code)]
fn describe() -> Value {
    json!({ "ops": ["plan", "apply", "restore", "backups", "clean"] })
}
