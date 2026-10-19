//! 双向合并的**宿主侧**：只留数据模型（`merge_model.rs`）+ 几个转发到插件的入口。
//!
//! 合并算法整个住在 `crates/merge-plugin`（编成 `plugin\merge-plugin.dll`）：
//! 这里没有任何合并、备份、还原逻辑，插件不在就调不动，功能也就不存在。

pub use crate::merge_model::*;

use std::path::Path;

use serde_json::json;

use crate::localdiff::Excludes;
use crate::svn::Svn;

/// 备份目录名前缀（与插件侧同名）
pub const BACKUP_PREFIX: &str = "merge-backup-";

/// 白名单：「参与合并的文件」那一格先过一遍与屏蔽框同一套缝合规则，再解析成匹配器。
/// 留空就换成内置的代码 / 文本清单（老配置不用迁移也能有个合理范围）；写了内容就只按写的走。
pub(crate) fn scope(raw: &str) -> Excludes {
    let rules = crate::commit::normalize_includes(raw);
    let rules = if rules.trim().is_empty() {
        crate::config::DEFAULT_MERGE_INCLUDE
    } else {
        &rules
    };
    Excludes::parse_includes(rules)
}

/// 黑名单：「不参与合并的文件」那一格，缝合同上；留空返回 `None`
pub(crate) fn blacklist(raw: &str) -> Option<Excludes> {
    let rules = crate::commit::normalize_excludes(raw);
    (!rules.trim().is_empty()).then(|| Excludes::parse(&rules))
}

fn conn(svn: Option<&Svn>) -> serde_json::Value {
    match svn {
        Some(svn) => json!({
            "exe": svn.exe.display().to_string(),
            "user": svn.username,
            "password": svn.password,
        }),
        None => json!({ "exe": "", "user": "", "password": "" }),
    }
}

/// 扫一遍差异、逐项定出动作（一个字节都不写）。**这一步在插件里跑**。
pub fn plan_with_state(
    local_root: &Path,
    server_root: &Path,
    filter: &str,
    include: &str,
    exclude: &str,
    svn: Option<&Svn>,
    limits: &MergeLimits,
    state_path: Option<&Path>,
) -> Result<Plan, String> {
    let mut payload = conn(svn);
    let map = payload.as_object_mut().expect("json 对象");
    map.insert("local_root".into(), json!(local_root.display().to_string()));
    map.insert("server_root".into(), json!(server_root.display().to_string()));
    map.insert("filter".into(), json!(filter));
    map.insert("include".into(), json!(include));
    map.insert("exclude".into(), json!(exclude));
    map.insert(
        "state_path".into(),
        match state_path {
            Some(path) => json!(path.display().to_string()),
            None => json!(null),
        },
    );
    map.insert("lcs_cell_limit".into(), json!(limits.lcs_cell_limit));
    let data = crate::plugin_host::call(crate::plugins::MERGE_DLL, "plan", payload)?;
    let id = data.get("id").and_then(|v| v.as_u64()).unwrap_or_default();
    let plan = data.get("plan").cloned().unwrap_or(serde_json::Value::Null);
    let mut plan: Plan =
        serde_json::from_value(plan).map_err(|e| format!("插件回的计划解不动：{e}"))?;
    plan.plan_id = id;
    plan.backup_bytes = data.get("backup_bytes").and_then(|v| v.as_u64()).unwrap_or_default();
    Ok(plan)
}

/// 按清单落盘（插件执行，含备份与回滚日志）
pub fn apply(plan: &Plan, root: &Path, backup: bool) -> Result<Applied, String> {
    let data = crate::plugin_host::call(
        crate::plugins::MERGE_DLL,
        "apply",
        json!({
            "plan_id": plan.plan_id,
            "root": root.display().to_string(),
            "backup": backup,
        }),
    )?;
    serde_json::from_value(data).map_err(|e| format!("插件回的执行结果解不动：{e}"))
}

/// 拿某一次备份整份还原
pub fn restore_with_client(dir: &Path, client: Option<&Svn>) -> Result<Restored, String> {
    let mut payload = conn(client);
    payload
        .as_object_mut()
        .expect("json 对象")
        .insert("backup".into(), json!(dir.display().to_string()));
    let data = crate::plugin_host::call(crate::plugins::MERGE_DLL, "restore", payload)?;
    serde_json::from_value(data).map_err(|e| format!("插件回的还原结果解不动：{e}"))
}

/// 历次备份清单，新 → 旧
pub fn backups(root: &Path) -> Vec<BackupInfo> {
    crate::plugin_host::call(crate::plugins::MERGE_DLL, "backups", json!({ "root": root.display().to_string() }))
        .ok()
        .and_then(|data| serde_json::from_value(data).ok())
        .unwrap_or_default()
}

/// 清掉历次备份，返回（目录数，释放字节数）
pub fn clean_backups(root: &Path) -> (usize, u64) {
    crate::plugin_host::call(crate::plugins::MERGE_DLL, "clean", json!({ "root": root.display().to_string() }))
        .ok()
        .map(|data| {
            (
                data.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                data.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0))
}
