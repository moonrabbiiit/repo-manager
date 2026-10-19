//! 双向合并的**数据模型**：主程序只认这份结构，合并算法本身在插件 DLL 里。
//!
//! 与 `crates/merge-plugin` 里的定义靠 JSON 对齐（字段名一致、serde 默认忽略多余字段）。
//! 插件回包里的 `validation_client` 这类只在插件内部用的字段不会出现在这里。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

/// 单个文件的字节上限（插件侧同名常量）
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// LCS 规模上限：祖先行数 × 变化侧行数，超过即退化为字节级兜底
pub const LCS_CELL_LIMIT: usize = 60_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub enum RegionKind {
    /// 只有本地侧改过：这一块的原文进服务器
    LocalOnly,
    /// 只有服务器侧改过：这一块的原文进本地
    ServerOnly,
    /// 两边各自做了同一处改动（内容与覆盖范围都一致）：只应用一次
    BothSame,
    /// 同一处两边改得不一样：整个文件不动，进通知清单
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub struct Region {
    /// 祖先行区间 `[lo, hi)`；纯插入时两端相同
    pub anc: (usize, usize),
    /// 改过的一侧在该文件里的行区间；没改的那一侧是 `None`
    pub local: Option<(usize, usize)>,
    pub server: Option<(usize, usize)>,
    pub kind: RegionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub enum WcState {
    /// 正常版本化（normal / modified）
    Normal,
    /// 计划新增（added）：还没有基线版本可用
    Added,
    /// 目录里有这个文件，但没进版本控制
    Unversioned,
    /// 工作副本里根本没有这个路径（本地新增要拷进去的目标）
    Absent,
    /// 计划删除（deleted）或文件已丢失（missing）
    Deleted,
    /// 冲突还没解决
    Conflict,
    /// ignored / external / incomplete 等其它状态
    Other,
}

impl WcState {
    /// 允许本功能写入的状态。`Deleted` 排在外面是刻意的：工作副本里计划删除的文件
    /// 一旦被拷回内容，`svn commit` 就不再删它了
    pub fn writable(self) -> bool {
        matches!(self, Self::Normal | Self::Added | Self::Unversioned | Self::Absent)
    }

    pub fn text(self) -> &'static str {
        match self {
            Self::Normal => "正常版本化",
            Self::Added => "计划新增",
            Self::Unversioned => "未版本化",
            Self::Absent => "不在工作副本里",
            Self::Deleted => "已计划删除",
            Self::Conflict => "冲突未解决",
            Self::Other => "非常规状态",
        }
    }
}

/// 定不出基线的几种原因。它们在他人机器上是完全不同的处置（补 update、开网络、
/// 还是干脆手工对），所以不并成一个「失败」
#[derive(Clone, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub enum Uncertain {
    /// 内容相近不代表共同祖先；没有可验证的同步记录或版本证据。
    Unproven,
    /// `-r BASE` 取不到：未版本化、路径不在工作副本里、或 svn 直接报错（附原文）
    NoBase(String),
    /// 往回扫历史时连不上服务器（附原文）
    Offline(String),
    /// 扫到了最接近的版本，但它和本地侧的差距仍大于容差
    TooFar { candidate: u64, distance: usize, threshold: usize },
    /// 窗口内没有任何版本比 BASE 更接近：岔口比扫描窗口更早
    WindowClosed { distance: usize, window: usize },
}

impl Uncertain {
    pub fn text(&self) -> String {
        match self {
            Self::Unproven => "没有可验证的共同基线，请先人工核对两侧内容".to_owned(),
            Self::NoBase(msg) => format!("取不到基线版本：{msg}"),
            Self::Offline(msg) => format!("定岔时连不上服务器：{msg}"),
            Self::TooFar { candidate, distance, threshold } => {
                format!("最接近的 r{candidate} 仍与本地侧差 {distance} 行，超出 {threshold} 行的容差")
            }
            Self::WindowClosed { distance, window } => {
                format!("往回 {window} 版内都没有比基线更接近的状态（与基线差 {distance} 行）")
            }
        }
    }
}

/// 本次合并给某个文件定出来的祖先
#[derive(Clone, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub enum AncInfo {
    /// 上次两侧一致、由程序保存的同步快照。
    Synced,
    /// 就是 `-r BASE`
    Base,
    /// 往回扫到的某个版本
    Rev(u64),
    /// 定不出来
    Unknown(Uncertain),
    /// 单侧新增 / 单侧缺失，本来就不需要祖先
    NotNeeded,
}

impl AncInfo {
    pub fn label(&self) -> String {
        match self {
            Self::Synced => "上次同步".to_owned(),
            Self::Base => "基线".to_owned(),
            Self::Rev(rev) => format!("r{rev}"),
            Self::Unknown(reason) => reason.text(),
            Self::NotNeeded => "—".to_owned(),
        }
    }
}

/// 祖先查询的抽象：`svn.rs` 里接 svn.exe，测试喂内存桩，所以合并逻辑不需要真网络
pub trait Ancestor: Send + Sync {
    fn metadata(&self) -> Result<Option<Metadata>, String> { Ok(None) }
    fn pending_properties(&self) -> Vec<String> { Vec::new() }
    fn binding(&self) -> Result<Option<RepositoryBinding>, String> { Ok(None) }
    fn validation_client(&self) -> Option<crate::svn::Svn> { None }
    /// 服务器端那个文件在工作副本里的状态；`exists` 是它在磁盘上是否真的存在
    /// （配对已经告诉我们了，不必让实现再 stat 一次）
    fn wc_state(&self, rel: &str, exists: bool) -> WcState;
    /// `-r BASE` 的原始字节；`None` = 这个文件没有基线版本，`Err` = svn 报错原文
    fn base(&self, rel: &str) -> Result<Option<Vec<u8>>, String>;
    /// 该文件不晚于 BASE 的历史版本，新 → 旧，最多 `max` 个。
    /// 空表 = 这个文件没有可比的历史；`Err` = 读提交记录时 svn 报错原文（连不上服务器、认证被拒），
    /// 两者在报告里是不同处置，不能混成一种
    fn revs(&self, rel: &str, max: usize) -> Result<Vec<u64>, String>;
    /// 某个版本上的原始字节
    fn at_rev(&self, rel: &str, rev: u64) -> Result<Option<Vec<u8>>, String>;
}

/// 一次 `plan` 里 svn 调用的记账：预算撞线就收尾，绝不假装跑完
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub enum Action {
    /// 行级双向合并，两边收敛成并集
    MergeBoth,
    /// 只有本地侧有：拷进工作副本（版本化交给「全部上传」的自动 `svn add`）
    CopyLocalToServer,
    /// 只有服务器端有：拷到本地开发侧
    CopyServerToLocal,
    /// 白名单内的非文本 / 超大文件：整文件覆盖服务器端
    OverwriteLocalToServer,
    /// 白名单内的非文本 / 超大文件：整文件覆盖本地侧
    OverwriteServerToLocal,
    /// 服务器端已删除该文件、本地还留着：合并时把本地副本也删掉（删前先备份）
    DeleteLocal,
    /// 本地已删除、服务器未修改：同步删除工作副本中的文件。
    DeleteServer,
    SkipMetadata,
    /// 同一处两边都改过：整文件一个字节都不动，只进通知清单
    SkipConflict,
    /// 定不出基线：不动
    SkipUndecided,
    /// 二进制档里两边都相对基线变过：不动
    SkipBinary,
    /// 白名单外：不参与合并
    SkipNotWhitelisted,
    /// 命中「不参与合并的文件」那一格：即使也在白名单里也不参与
    SkipBlacklisted,
    /// 合完与现状相同（含两边做了同一处改动）
    SkipUnchanged,
}

impl Action {
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Self::MergeBoth
                | Self::CopyLocalToServer
                | Self::CopyServerToLocal
                | Self::OverwriteLocalToServer
                | Self::OverwriteServerToLocal
                | Self::DeleteLocal
                | Self::DeleteServer
        )
    }

    pub fn text(self) -> &'static str {
        match self {
            Self::MergeBoth => "双向合并",
            Self::CopyLocalToServer => "拷入服务器端",
            Self::CopyServerToLocal => "拷到本地侧",
            Self::OverwriteLocalToServer => "覆盖服务器端",
            Self::OverwriteServerToLocal => "覆盖本地侧",
            Self::DeleteLocal => "服务器已删除",
            Self::DeleteServer => "本地已删除",
            Self::SkipMetadata => "保留 SVN 属性",
            Self::SkipConflict => "冲突",
            Self::SkipUndecided => "无基线",
            Self::SkipBinary => "两边都改",
            Self::SkipNotWhitelisted => "白名单外",
            Self::SkipBlacklisted => "黑名单内",
            Self::SkipUnchanged => "已一致",
        }
    }
}

/// 写入目标侧。备份目录、journal 与 rel 都按这一侧自己走
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub enum Target {
    Local,
    Server,
}

impl Target {
    fn dir(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

/// 文件的size+mtime 指纹：预览是建议不是锁，落盘前逐个重查
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub struct Finger {
    pub size: u64,
    pub modified: u64,
}

impl Finger {
    fn now(path: &Path) -> Option<Finger> {
        let meta = fs::metadata(path).ok()?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or_default();
        Some(Finger { size: meta.len(), modified })
    }
}

#[derive(Clone, Debug)]
#[derive(Serialize, Deserialize)]
pub struct Entry {
    /// 小写配对键：清单去重与备份目录用它
    pub rel: String,
    /// 展示用的相对路径（本地写法优先）
    pub display: String,
    pub local_rel: String,
    pub server_rel: String,
    pub action: Action,
    pub ancestor: AncInfo,
    pub regions: Vec<Region>,
    pub local: Option<Finger>,
    pub server: Option<Finger>,
    /// 预览时算好的字节：落盘写出去的就是这份，apply 不再重算（也不再走网络）
    #[serde(default)]
    pub write_local: Option<Vec<u8>>,
    #[serde(default)]
    pub write_server: Option<Vec<u8>>,
    /// 服务器端已删除、要同步删掉本地这份（与 `write_local` 互斥：删和写不并存）
    pub delete_local: bool,
    pub delete_server: bool,
    /// 预览实际读取的两侧字节；缺失也必须在应用时重新核对。
    #[serde(default)]
    pub observed_local: Option<Vec<u8>>,
    #[serde(default)]
    pub observed_server: Option<Vec<u8>>,
    pub note: String,
}

impl Entry {
    pub fn targets(&self) -> Vec<Target> {
        let mut out = Vec::new();
        if self.write_local.is_some() || self.delete_local {
            out.push(Target::Local);
        }
        if self.write_server.is_some() || self.delete_server {
            out.push(Target::Server);
        }
        out
    }

    /// 预览行里的「改动区间」列：合并的按侧计数，冲突的给行号——要人工处理看的就是行号
    pub fn regions_text(&self) -> String {
        if self.action == Action::SkipConflict {
            let items: Vec<String> = self
                .regions
                .iter()
                .filter(|r| r.kind == RegionKind::Conflict)
                .take(3)
                .map(|r| {
                    let l = r.local.map(|(a, b)| format!("本地 {}–{}", a + 1, b.max(a + 1)));
                    let s = r.server.map(|(a, b)| format!("服务器 {}–{}", a + 1, b.max(a + 1)));
                    match (l, s) {
                        (Some(l), Some(s)) => format!("{l} / {s}"),
                        (Some(l), None) | (None, Some(l)) => l,
                        (None, None) => String::new(),
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
            let more = self.regions.iter().filter(|r| r.kind == RegionKind::Conflict).count() - items.len();
            let mut text = items.join(" · ");
            if more > 0 {
                text.push_str(&format!(" 等 {more} 处"));
            }
            return text;
        }
        let local = self.regions.iter().filter(|r| r.kind == RegionKind::LocalOnly).count();
        let server = self.regions.iter().filter(|r| r.kind == RegionKind::ServerOnly).count();
        let same = self.regions.iter().filter(|r| r.kind == RegionKind::BothSame).count();
        if local + server + same == 0 {
            return String::new();
        }
        let mut parts = Vec::new();
        if local > 0 {
            parts.push(format!("本地 {local} 处"));
        }
        if server > 0 {
            parts.push(format!("服务器 {server} 处"));
        }
        if same > 0 {
            parts.push(format!("相同 {same} 处"));
        }
        parts.join(" · ")
    }

    /// 预览行 / 报告里的处置短标签。`SkipUndecided` 的原因各不相同：差异过多（岔得太远）、
    /// 取不到基线、连不上服务器——单列成「无基线」会误导，按实际原因给标签
    pub fn action_text(&self) -> String {
        match self.action {
            Action::SkipUndecided => match &self.ancestor {
                AncInfo::Unknown(Uncertain::TooFar { .. })
                | AncInfo::Unknown(Uncertain::WindowClosed { .. }) => "差异过多".to_owned(),
                AncInfo::Unknown(Uncertain::NoBase(_)) => "无基线".to_owned(),
                AncInfo::Unknown(Uncertain::Offline(_)) => "连不上".to_owned(),
                AncInfo::Unknown(Uncertain::Unproven) => "无共同基线".to_owned(),
                _ => "待人工".to_owned(),
            },
            // 「双向合并」的实际方向可能是单边：只服务器改过就是要合入本地、只本地改过就是
            // 合入服务器。全叫「双向合并」会让人以为「连我没动的文件也被算成我改的」，
            // 按写的是哪一侧分明方向。
            Action::MergeBoth => match (self.write_local.is_some(), self.write_server.is_some()) {
                (true, false) => "合入本地".to_owned(),
                (false, true) => "合入服务器".to_owned(),
                _ => "双向合并".to_owned(),
            },
            other => other.text().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default)]
#[derive(Serialize, Deserialize)]
pub struct Plan {
    pub entries: Vec<Entry>,
    /// 插件里那份完整计划（含字节）的编号；落盘时按它取，清单本身不搬字节
    #[serde(default)]
    pub plan_id: u64,
    /// 备份 + 写入快照的总字节数，插件算好带过来（页脚那句「本次备份约 1.2 MB」）
    #[serde(default)]
    pub backup_bytes: u64,
    pub local_root: PathBuf,
    pub server_root: PathBuf,
    /// 本次真读过内容的对数（含一致的）
    pub checked: usize,
    /// 该读但预算用满没读的对数：差异数因此是下界
    pub unverified: usize,
    /// 白名单外的差异项：单独计数，不静默消失
    pub outside_whitelist: usize,
    /// 命中黑名单而被挡掉的差异项：同样单独计数，让它看得见「这条规则真的起作用了」
    pub blacklisted: usize,
    /// 时间或 svn 调用预算撞线后没来得及判定的条目数
    pub untried: usize,
    pub budget_hit: bool,
    /// 两侧上次一致的快照，存放于配置目录，与备份独立。
    pub state_path: Option<PathBuf>,
    pub state_revision: u64,
    pub server_binding: Option<RepositoryBinding>,
    pub local_binding: Option<RepositoryBinding>,
    pub observed_states: HashMap<String, (WcState, WcState)>,
    pub binding_reset: bool,
    pub metadata: (Option<Metadata>, Option<Metadata>),
}

pub type Metadata = HashMap<String, std::collections::BTreeMap<String, String>>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryBinding {
    pub uuid: String,
    pub url: String,
}

impl Plan {
    fn counted(&self, action: Action) -> usize {
        self.entries.iter().filter(|e| e.action == action).count()
    }

    /// 会写入的文件数
    pub fn writing(&self) -> usize {
        self.entries.iter().filter(|e| e.action.is_write()).count()
    }

    pub fn line_merges(&self) -> usize {
        self.counted(Action::MergeBoth)
    }

    pub fn copies(&self) -> usize {
        self.counted(Action::CopyLocalToServer) + self.counted(Action::CopyServerToLocal)
    }

    pub fn overwrites(&self) -> usize {
        self.counted(Action::OverwriteLocalToServer) + self.counted(Action::OverwriteServerToLocal)
    }

    /// 服务器端已删除、要跟着删掉的本地条目数
    pub fn deletes(&self) -> usize {
        self.counted(Action::DeleteLocal) + self.counted(Action::DeleteServer)
    }

    pub fn conflicts(&self) -> usize {
        self.counted(Action::SkipConflict)
    }

    pub fn undecided(&self) -> usize {
        self.counted(Action::SkipUndecided)
    }

    /// 白名单内的非文本 / 超大文件，且两边都相对基线改过：谁都不覆盖谁
    pub fn binaries(&self) -> usize {
        self.counted(Action::SkipBinary)
    }

    /// 两格范围规则挡掉的总条数：页脚只报这一个数，分成白 / 黑两处的明细在逐行清单和导出报告里
    pub fn out_of_scope(&self) -> usize {
        self.outside_whitelist + self.blacklisted
    }

    /// 判下来两边已经一致的：不写也不占人工清单，只报个数
    pub fn agreed(&self) -> usize {
        self.counted(Action::SkipUnchanged)
    }

    pub fn skipping(&self) -> usize {
        self.entries.iter().filter(|e| !e.action.is_write()).count()
    }

}

/// 规模与预算。`max_time` 沿用差异核对那套 60 秒收口口径
#[derive(Clone, Debug)]
pub struct MergeLimits {
    pub max_file_bytes: u64,
    pub max_plan_bytes: u64,
    pub max_time: Duration,
    pub max_files: usize,
    pub svn_calls: usize,
    /// 往回扫多少版
    pub history_max: usize,
    /// 距离阈值：超过就判「定不出来」
    pub give_up_lines: usize,
    /// LCS 规模上限（祖先行数 × 变化侧行数）。超过即判 too_big，退化为字节级单方改写兜底。
    /// 默认 `LCS_CELL_LIMIT`；做成字段是为了能从 config.json 调，不必重编译
    pub lcs_cell_limit: usize,
}

impl Default for MergeLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            max_plan_bytes: 256 * 1024 * 1024,
            max_time: Duration::from_secs(60),
            max_files: 20_000,
            svn_calls: 400,
            history_max: 15,
            give_up_lines: 24,
            lcs_cell_limit: LCS_CELL_LIMIT,
        }
    }
}



/// 把清单导出成文字。页脚按实说「未核对 / 撞线后没判定」，不写成「已全部合并」
pub fn report(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!("本地开发侧：{}\n", plan.local_root.display()));
    out.push_str(&format!("服务器端：{}\n\n", plan.server_root.display()));
    out.push_str(&format!(
        "写入 {} 项（行级 {} · 整文件覆盖 {} · 新增拷 {} · 删除 {}）\n",
        plan.writing(),
        plan.line_merges(),
        plan.overwrites(),
        plan.copies(),
        plan.deletes()
    ));
    out.push_str(&format!(
        "跳过 {} 项（冲突 {} · 待人工 {} · 两边都改的非文本 {} · 白名单外 {} · 黑名单内 {} · 已一致 {}）\n",
        plan.skipping(),
        plan.conflicts(),
        plan.undecided(),
        plan.binaries(),
        plan.outside_whitelist,
        plan.blacklisted,
        plan.agreed()
    ));
    out.push_str(&format!(
        "已核对 {} 项 · 未核对内容 {} 项 · 本次备份约 {}\n\n",
        plan.checked,
        plan.unverified,
        human_bytes(plan.backup_bytes)
    ));
    for entry in &plan.entries {
        if entry.action == Action::SkipUnchanged {
            continue;
        }
        let line = format!(
            "{}\t{}\t{}\t{}",
            entry.action_text(),
            entry.ancestor.label(),
            entry.display,
            entry.regions_text()
        );
        let line = if entry.note.is_empty() {
            line
        } else {
            format!("{line}\t{}", entry.note)
        };
        out.push_str(&line);
        out.push('\n');
    }
    if plan.budget_hit {
        out.push_str(&format!("\n预算用满，另有 {} 项没来得及判定。\n", plan.untried));
    }
    out
}

#[derive(Clone, Debug, Default)]
#[derive(Serialize, Deserialize)]
pub struct Applied {
    /// 这次会话的备份目录；选了「直接合并」就没有还原点，这里是 `None`
    pub backup: Option<PathBuf>,
    pub written: usize,
    pub bytes: u64,
    pub failed: Vec<String>,
}

/// 还原的结果
#[derive(Clone, Debug, Default)]
#[derive(Serialize, Deserialize)]
pub struct Restored {
    pub restored: usize,
    pub removed: usize,
    pub failed: Vec<String>,
}

/// 一次会话备份的清单，给「还原上一次合并」那扇窗列出来
#[derive(Clone, Debug)]
#[derive(Serialize, Deserialize)]
pub struct BackupInfo {
    pub dir: PathBuf,
    pub name: String,
    pub files: usize,
    pub bytes: u64,
    pub unfinished: bool,
}



/// 备份量与写入量的口头尺寸：预览页脚要说「本次备份约 1.2 MB」
pub fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn root_key(path: &Path) -> String {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy().replace('\\', "/").to_lowercase()
}

pub fn sync_state_path(root: &Path, local: &Path, server: &Path) -> PathBuf {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (root_key(local), root_key(server)).hash(&mut hash);
    root.join("merge-state").join(format!("{:016x}", hash.finish())).join("state.json")
}

