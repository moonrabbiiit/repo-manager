//! 插件的 C ABI：一个 `abi` 查询 + 一个 JSON RPC 入口。
//!
//! 主程序用 `libloading` 按名取符号，调用完用 `rm_plugin_free` 还内存。
//! 请求/回包都是 UTF-8 JSON；回包固定 `{"ok":true,"data":...}` 或 `{"ok":false,"error":"..."}`。

use std::ffi::{c_char, CString};

use serde::Deserialize;
use serde_json::json;

use crate::ai;
use crate::APP_VERSION;

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
            "id": "ai-log",
            "name": "AI 工作日志",
            "version": APP_VERSION,
            "abi": ABI_VERSION,
        })),
        "generate" => generate(payload),
        _ => err(format!("不认识的操作：{op}")),
    }
}

#[derive(Deserialize)]
struct GenerateRequest {
    #[serde(default)]
    url: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    tone: String,
    #[serde(default)]
    extra: String,
    #[serde(default)]
    content: String,
}

fn generate(payload: &str) -> String {
    let req: GenerateRequest = match serde_json::from_str(payload) {
        Ok(req) => req,
        Err(e) => return err(format!("参数不对：{e}")),
    };
    match ai::request(
        &req.url, &req.key, &req.model, &req.tone, &req.extra, &req.content,
    ) {
        Ok(text) => ok(json!({ "text": text })),
        Err(e) => err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn info_reports_the_plugin_identity() {
        let value: Value = serde_json::from_str(&dispatch("info", "")).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["id"], "ai-log");
        assert_eq!(value["data"]["abi"], ABI_VERSION);
    }

    #[test]
    fn an_unknown_op_is_reported_not_ignored() {
        let value: Value = serde_json::from_str(&dispatch("nope", "")).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap().contains("不认识"));
    }

    /// 参数缺省时走的是「没配置服务地址」这条正常校验，而不是解析失败
    #[test]
    fn generate_validates_the_configuration_first() {
        let value: Value = serde_json::from_str(&dispatch("generate", "{}")).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap().contains("服务地址"));
    }
}
