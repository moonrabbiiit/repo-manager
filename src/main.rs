#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod bcompare;
mod childproc;
mod commit;
mod compare;
mod config;
mod daterange;
mod dialogs;
mod fonts;
mod git;
mod header;
mod history;
mod home;
mod icons;
mod jobs;
mod localdiff;
mod merge;
mod merge_model;
mod notify;
mod plugin_host;
mod plugins;
mod repo;
mod settings;
mod svn;
mod tasks;
mod titlebar;
mod ui;
mod update;
mod worklog;
#[cfg(test)]
mod testbed;

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use egui::{Color32, Ui};

use crate::commit::CommitPage;
use crate::config::Config;
use crate::history::{FileDiff, FileLog, HistoryPage};
use crate::jobs::Pool;
use crate::svn::{Svn, WcInfo};

// 通用绘制工具搬到 ui.rs，这里转出一层，history / commit / daterange 里的 crate::ink 照旧可用
pub use ui::{
    field_margin, highlight, highlight_with, ink, pale, pane_fill, pane_hover, pane_stroke, quiet,
    Search, INK, ACCENT, AMBER, DANGER_FILL, DANGER_TEXT, ERR, OK,
};

pub const APP_TITLE: &str = "仓库管理器";
/// 程序版本号，由 build.rs 从根目录 version.txt 注入；发布新版本时只改那个文件
pub const APP_VERSION: &str = env!("REPO_MANAGER_VERSION");

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Command,
    Success,
    Warning,
    Error,
}

impl Level {
    pub(crate) fn color(self, ui: &Ui) -> Color32 {
        ink(
            ui,
            match self {
                Self::Info => Color32::from_gray(200),
                Self::Command => Color32::from_rgb(120, 190, 240),
                Self::Success => crate::ui::OK,
                Self::Warning => crate::ui::AMBER,
                Self::Error => crate::ui::ERR,
            },
        )
    }
}

/// 「修改仓库地址」窗口状态：把工作副本 URL 里的 `from` 前缀换成 `to` 前缀。
#[derive(Clone)]
pub struct Relocate {
    pub dir: usize,
    /// 原地址前缀，默认填仓库根地址
    pub from: String,
    /// 新地址前缀
    pub to: String,
    pub focus: bool,
    /// 上一次执行的失败原因
    pub error: String,
}

/// 「全部上传」窗口状态：所有目录共用一条说明，各自起一个提交任务。
#[derive(Clone, Default)]
pub struct UploadAll {
    pub message: String,
    /// 第一下只是把按钮变成「确认」，再点一次才真的提交（这一步会改动服务器）
    pub confirm: bool,
    /// 打开时抢一下输入框焦点
    pub focus: bool,
    /// 每个目录是否参与本次批量上传（与 cfg.dirs 对齐，新勾进的默认选中）
    pub checked: Vec<bool>,
    /// 正在查看「待提交明细」的目录（None = 明细窗口关着）
    pub detail: Option<usize>,
}

/// 「批量修改仓库」窗口状态：多个目录共用一对前缀，各自起一个改地址任务。
#[derive(Clone, Default)]
pub struct BatchRelocate {
    /// 要替换掉的旧地址前缀
    pub from: String,
    /// 新地址前缀
    pub to: String,
    /// 每个目录是否参与本次批量修改仓库（与 cfg.dirs 对齐）
    pub checked: Vec<bool>,
    /// from 上一次的值：变了就按新前缀重算一遍默认勾选
    pub last_from: String,
    /// 打开时抢一下 from 输入框的焦点
    pub focus: bool,
    /// 第一下只是把按钮变成「确认」，再点一次才真的发任务
    pub confirm: bool,
}

/// 「批量分组 / 标记」窗口状态：给勾选的目录统一设置分组或标记。
/// 两个动作互不牵连，改的是本地配置、随时能改回，不做两步确认
#[derive(Clone, Default)]
pub struct BatchLabel {
    /// 每个目录是否参与（与 cfg.dirs 对齐，默认全不勾）
    pub checked: Vec<bool>,
    /// 要统一设置的分组名（空 = 清除所选目录的分组）
    pub group: String,
    /// 要统一设置的标记文字（空 = 清除标记）
    pub tag: String,
    /// 标记颜色，形如 "#RRGGBB"；空 = 按默认蓝显示（解析口径同 tag_color_rgb）
    pub tag_hex: String,
    /// 打开时抢一下分组输入框的焦点
    pub focus: bool,
}

/// 目录行「更多 → 差异过滤与提交屏蔽」的窗口状态。四格都是本目录的配置：过滤条件写
/// `DirConfig::bc_filter`（只管本地差异比对）、屏蔽规则写 `DirConfig::commit_excludes`
/// （决定会不会被提交）、合并范围两格写 `merge_include` 与 `merge_exclude`。四格分两页标签
/// 摆放，切页只动 `page`。窗口里另存一份过滤条件的行视图：配置里是分号串，
/// 框里要一行一条。
#[derive(Clone)]
pub struct CommitRules {
    /// 打开窗口那一刻提交页所在的目录
    pub dir: usize,
    /// 当前停在哪一页：0 = 差异过滤与提交屏蔽，1 = 双向合并范围
    pub page: usize,
    /// `bc_filter` 的行视图：编辑期间以它为准，落盘时按 `;` 拼回分号串
    pub filter_buf: String,
    /// 窗口开着时过滤条件改过：关窗口那一刻按新口径重算一次本地差异
    pub filter_dirty: bool,
    /// 打开时抢一下过滤框的焦点
    pub focus: bool,
}

impl CommitRules {
    pub(crate) fn new(dir: usize, bc_filter: &str) -> Self {
        Self {
            dir,
            page: 0,
            filter_buf: crate::commit::bc_filter_lines(bc_filter),
            filter_dirty: false,
            focus: true,
        }
    }
}

/// 「本地开发侧 ↔ 服务器端双向合并」的预览窗口。清单只待在内存里：它带着每个文件两边
/// 要写的字节，落盘按这份清单走，重启后重算一份才是真的现状。
#[derive(Default)]
pub struct MergePreview {
    pub dir: usize,
    /// `None` = 还在算，或上一次预览失败了
    pub plan: Option<crate::merge::Plan>,
    /// 第一下只把按钮换成「确认写入并备份」，再点一次才真的动两边
    pub confirm: bool,
    /// 上一次预览 / 落盘没成的原因
    pub error: String,
    /// 历史备份清单，展开「还原」那一栏时才去扫备份目录
    pub backups: Vec<crate::merge::BackupInfo>,
    pub show_backups: bool,
    /// 最大化 / 还原，与两个明细窗口共用一套
    pub zoom: crate::history::Zoom,
    /// 「清理备份」的第一下只把确认亮出来，再点一次才真删
    pub wipe: bool,
}

/// 「回退到某个版本」确认窗口：把整个目录或单个文件倒回所选版本，改动先落在本地。
#[derive(Clone)]
pub struct Rollback {
    /// 目录行（也是任务池里的 dir）
    pub dir: usize,
    /// 要回到的版本号（不带 r，直接给 svn merge 用）
    pub revision: String,
    /// 真正执行 `svn merge` 的本地路径
    pub target: PathBuf,
    /// 窗口标题与收尾文案里的对象名（目录别名 / 仓库相对路径）
    pub name: String,
    /// true = 整个目录，false = 单个文件
    pub is_dir: bool,
    /// 第一下只把按钮换成「确认回退」，再点一次才真的动工作副本
    pub confirm: bool,
    /// 上一次执行的失败原因
    pub error: String,
    /// 打开窗口那一刻该目录未解决的冲突 / 不完整条目数。回退照常执行，
    /// 只是要在窗口里说清：这些没解决的会被一起卷进回退结果
    pub conflicts: usize,
}

/// 「解决冲突」窗口：列出该目录里必须先人工处理的条目，每条挑一个收场策略，
/// 勾上的那批一次执行。策略都只动本地工作副本，不碰服务器。
#[derive(Clone, Default)]
pub struct Resolve {
    /// 目录行（也是任务池里的 dir）
    pub dir: usize,
    pub rows: Vec<ResolveRow>,
    /// 列表上方「全部取用」那一行的策略，点一下套到所有行
    pub bulk: crate::svn::Accept,
    /// 不完整（~）的条目数：resolve 动不了它们，只报一句「先更新这个目录」
    pub incomplete: usize,
    /// 第一下只把按钮换成「确认执行」，再点一次才真的动工作副本
    pub confirm: bool,
    /// 上一次执行的失败原因
    pub error: String,
}

/// 窗口里的一行 = 一个必须人工处理的条目
#[derive(Clone)]
pub struct ResolveRow {
    /// 本地绝对路径，`svn resolve` / `svn revert` 作用在它上
    pub target: PathBuf,
    /// 显示用文件名，整路径挂在 tooltip 上
    pub name: String,
    pub accept: crate::svn::Accept,
    pub checked: bool,
}

impl Resolve {
    /// status 结果 → 一版新行：只认真冲突（C），默认全选、默认「保留手头这份」
    pub fn from_entries(dir: usize, entries: &[crate::svn::StatusEntry]) -> Self {
        Resolve {
            dir,
            rows: Self::rows_of(entries),
            bulk: crate::svn::Accept::default(),
            incomplete: entries.iter().filter(|e| e.item == crate::svn::Item::Incomplete).count(),
            confirm: false,
            error: String::new(),
        }
    }

    fn rows_of(entries: &[crate::svn::StatusEntry]) -> Vec<ResolveRow> {
        entries
            .iter()
            .filter(|entry| entry.item == crate::svn::Item::Conflict)
            .map(|entry| ResolveRow {
                target: PathBuf::from(&entry.path),
                name: entry.name.clone(),
                accept: crate::svn::Accept::default(),
                checked: true,
            })
            .collect()
    }

    /// status 重读之后套一遍：还在冲突的保住他挑好的策略和勾，已经解决掉的从列表里消失。
    /// 窗口开着时人在 BC 里手动合完了，不能让他对着一条早就不存在的旧记录点执行。
    pub fn rebuild(&mut self, entries: &[crate::svn::StatusEntry]) {
        let picked = std::mem::take(&mut self.rows);
        let mut fresh = Self::rows_of(entries);
        for row in fresh.iter_mut() {
            if let Some(old) = picked.iter().find(|old| old.target == row.target) {
                row.accept = old.accept;
                row.checked = old.checked;
            }
        }
        self.rows = fresh;
        self.incomplete = entries
            .iter()
            .filter(|e| e.item == crate::svn::Item::Incomplete)
            .count();
    }

    /// 勾了行且至少一行选了「丢弃本地改动」——revert 会把改动整个扔掉，值得多问一句
    pub fn drops_local(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.checked && row.accept == crate::svn::Accept::Revert)
    }
}

#[derive(Clone)]
pub struct OutLine {
    pub level: Level,
    pub text: String,
}

/// 目录的运行期状态，与 `cfg.dirs` 一一对应。
#[derive(Clone, Default)]
pub struct DirView {
    /// 后端类型：目录里有 .git 走 Git，有 .svn 走 SVN（加目录时先按磁盘判定，
    /// 检测时再核一遍；检测前默认按 SVN 显示）
    pub kind: crate::repo::RepoKind,
    pub info: Option<WcInfo>,
    pub remote: Option<bool>,
    pub remote_msg: String,
    pub remote_rev: String,
    /// 服务器上该地址的最近一次提交（来自 `svn info <url>`）
    pub last_rev: String,
    pub last_author: String,
    pub last_date: String,
    /// 待提交条目数（口径同「全部上传」：修改 / 新增 / 删除，以及 ? 与 !）
    pub changed: Option<usize>,
    /// 待提交条目明细，来自最近一次检测或「待提交 X 项」点开的现读
    pub changes: Vec<crate::svn::StatusEntry>,
    /// 上面这一份的来源：最近一次读取到的**全部**改动条目，本目录屏蔽规则挡掉的也在里面。
    /// 「过滤与屏蔽规则」窗口边写规则边算命中数就靠它，`changes` 里那些已经滤干净了
    pub all_changes: Vec<crate::svn::StatusEntry>,
    /// 服务器上已变、本地还没更新的文件数（`svn status -u`，比本地/远端版本号可靠）
    pub out_of_date: Option<usize>,
    /// 需要人工处理的条目数（冲突 / 不完整）；`None` = 还没读到过，不显示
    pub conflicts: Option<usize>,
    /// 上面这些条目本身（口径同 `conflicts`）：「解决冲突」窗口列的就是它。
    /// `changes` 只留可提交的条目，冲突在那里被滤掉了，所以另存一份
    pub blocked: Vec<crate::svn::StatusEntry>,
    /// 本地端（「绑定对比对象」里那个不受版本控制的目录）与这个工作副本的差异；
    /// `None` = 没绑定、目录读不到或还没测过——和 `changed` 一样，不显示，绝不显示成 0
    pub local_diff: Option<crate::localdiff::Summary>,
    /// 上面这个数落地的时间：本地端比对是独立任务，比 svn 检测晚几秒到
    pub local_checked_at: String,
    pub checked_at: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Main,
    Commit,
    History,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Maintain {
    Add,
    Delete,
    Cleanup,
    Revert,
}

impl Maintain {
    pub fn label(self) -> &'static str {
        match self {
            Self::Add => "加入版本控制",
            Self::Delete => "标记删除",
            Self::Cleanup => "清理工作副本",
            Self::Revert => "撤销修改",
        }
    }
}

pub struct RepoApp {
    pub cfg: Config,
    pub svn: Svn,
    /// 主窗口的 HWND（启动时取一次，给 DWM 设标题栏配色用）；拿不到就是 None
    pub hwnd: Option<isize>,
    /// 上一次写给原生标题栏的深浅态，只有它变了才重设一次 DWM 属性
    pub caption_dark: Option<bool>,
    /// 上一次应用圆角时的最大化状态：非最大化四角圆角、最大化直角，变了才发一次 DWM
    pub corner_maximized: Option<bool>,
    /// Git 命令行（git.exe），给 .git 目录行用
    pub git: crate::git::Git,
    /// git.exe 的版本号（设置窗口与状态行显示）
    pub git_version: Option<String>,
    /// Beyond Compare（BCompare.exe）路径，未找到时为空
    pub bc: PathBuf,
    pub version: Option<String>,
    pub candidates: Vec<String>,
    pub probing: bool,
    pub refresh_after_detect: bool,
    /// 本机 svn 登录人（取自 `svn auth`，取不到时退回 Windows 账户名）
    pub svn_user: String,
    pub user_probed: bool,
    pub dirs: Vec<DirView>,
    pub pool: Pool,
    pub output: Vec<OutLine>,
    pub page: Page,
    pub commit: Option<CommitPage>,
    pub history: Option<HistoryPage>,
    /// 双击历史页「涉及文件」后打开的逐行改动窗口（左右分栏）
    pub file_diff: Option<FileDiff>,
    /// 点「查看提交记录」后打开的单个文件提交记录窗口
    pub file_log: Option<FileLog>,
    pub selected: Option<usize>,
    pub new_path: String,
    pub new_label: String,
    /// 正在修改别名的目录行
    pub edit_label: Option<usize>,
    pub label_buf: String,
    pub label_focus: bool,
    /// 正在修改分组的目录行
    pub edit_group: Option<usize>,
    pub group_buf: String,
    pub group_focus: bool,
    /// 正在修改标记的目录行
    pub edit_tag: Option<usize>,
    pub tag_buf: String,
    pub tag_focus: bool,
    /// 正在被拖动的目录行（None = 没在拖）。拖到哪、插在哪都靠下面几项判
    pub drag: Option<usize>,
    /// 按下的候选行（第 1 个 = 哪一行，第 2 个 = 按下时的 i.time）：按住够久才转成 drag
    pub drag_pending: Option<(usize, f64)>,
    /// 拖动中的指针位置：幽灵标签跟着它走
    pub drag_pos: Option<egui::Pos2>,
    /// 指针当前命中的落点，只用来画插入线和把标题条点亮
    pub drag_target: Option<home::DropZone>,
    /// 上一帧画出来的落点区域（全局坐标）：分组标题条整条、目录行或卡片各切两半。
    /// 反查用上一帧的：拖动过程中列表一行都不挪，坐标照样准，而本帧的要等画完才齐
    pub drop_zones: Vec<(home::DropZone, egui::Rect)>,
    /// 正在修改仓库地址的目录（None = 窗口关闭）
    pub relocate: Option<Relocate>,
    /// 正在批量上传的目录清单（None = 窗口关闭）
    pub upload_all: Option<UploadAll>,
    /// 「批量修改仓库」窗口（None = 窗口关闭）
    pub batch_relocate: Option<BatchRelocate>,
    /// 「批量分组 / 标记」窗口（None = 窗口关闭）
    pub batch_label: Option<BatchLabel>,
    /// 正在确认的版本回退（None = 窗口关闭）
    pub rollback: Option<Rollback>,
    /// 「解决冲突」窗口（None = 窗口关闭）
    pub resolve: Option<Resolve>,
    /// 「本地开发侧 ↔ 服务器端双向合并」窗口（None = 窗口关闭）
    pub merge: Option<MergePreview>,
    /// 「差异过滤与提交屏蔽」窗口，目录行「更多」里开（None = 窗口关闭）
    pub commit_rules: Option<CommitRules>,
    pub show_settings: bool,
    /// 「插件」面板开关：顶栏拼图图标点开，独立窗口（不再塞在设置页里）
    pub show_plugins: bool,
    /// 测试用：覆盖「插件在不在」的判断（None = 照实看 dll 文件）
    pub plugin_present: Option<bool>,
    pub confirm_remove: Option<usize>,
    pub auto_scroll: bool,
    pub next_refresh: Instant,
    /// 请求过但当时同目录已有一次检测在跑，等那次落地之后补跑一次
    pub pending_refresh: Vec<(usize, bool)>,
    /// 更新通知订阅：本帧内检测到「有新版本且未通知过」的目录先收集到这里，
    /// 帧末合并成一条系统通知发出（不逐目录连环弹），发完即清空
    pub notify_queue: Vec<crate::notify::UpdateNotice>,
    pub font_note: String,
    /// 最新版本信息（官方 GitHub 发布或自建服务端的 latest.json，检查过更新才有）
    pub update_info: Option<update::UpdateManifest>,
    /// 更新源上确实有可装的新东西：按文件 sha256 判定（见 `update::has_update`，
    /// 同一个版本号重新发布的构建也算），检查那一趟的后台线程算好后放这里，
    /// 界面每帧只读它，不重算哈希
    pub update_ready: bool,
    /// 启动后第一次检查更新的时刻（None = 已触发过）
    pub update_check_at: Option<Instant>,
    /// 开了「自动检查更新」后的下一次检查时刻（每 5 分钟一轮）
    pub next_update_check: Instant,
    /// 「发现新版本」确认对话框开关（header 与设置里的更新入口都走它）
    pub show_update_confirm: bool,
    /// 最近一次下载新版本失败的原因（None = 没在失败状态）；对话框里内联显示
    pub update_error: Option<String>,
    /// AI 工作日志：从各目录历史页勾选的本人提交（快照，跨页面保留）
    pub ai_picks: Vec<worklog::AiPick>,
    /// AI 日志窗口里「补充其他目录」下拉框当前选中的目录编号
    pub ai_pick_dir: usize,
    /// AI 工作日志窗口开关
    pub show_worklog: bool,
    /// 日志模式开关：开启后各目录「提交记录」页里本人提交的版本行才出现勾选框
    ///（右上角「AI 日志」按钮以开关形式控制，见 header）
    pub ai_mode: bool,
    /// AI 工作日志：额外提示词（只在当前会话保留，不写配置）
    pub ai_extra: String,
    /// AI 工作日志：最近一次生成的日志正文（可直接编辑）
    pub ai_result: String,
    /// AI 工作日志：最近一次生成失败的原因
    pub ai_error: String,
    /// 右下角那张贴图：启动时把嵌进 exe 的 PNG 解一次，解不出来就一直是 None（不画）
    pub mascot: Option<egui::TextureHandle>,
    /// 顶栏标题左边的软件标志：与窗口图标同源（rabbit.png），解不出来就不画
    pub logo: Option<egui::TextureHandle>,
    /// 右上角与窗口控制键的 SVG 图标（启动时栅格化一次；无头测试里 None = 不画按钮区）
    pub icons: Option<crate::icons::IconSet>,
}

/// 取 icon/repo-manager.ico 里 48px 那张当窗口图标（标题栏 + 任务栏）。
/// 直接读嵌进 exe 的同一个 ico，图标就只有 rabbit.png -> repo-manager.ico 这一条来源。
/// 解析不出来就返回 None，让 eframe 用它的默认图标，不影响启动。
fn window_icon() -> Option<egui::IconData> {
    icon_rgba(include_bytes!("../icon/repo-manager.ico"))
}

/// 从 ico 字节里解出 48px 那张。ico 里的图是 32bpp 未压缩位图，像素自下而上存、
/// 字节序 BGRA，所以要翻转并换成 RGBA。
fn icon_rgba(ico: &[u8]) -> Option<egui::IconData> {
    const EDGE: usize = 48;
    let count = u16::from_le_bytes(ico.get(4..6)?.try_into().ok()?) as usize;
    for index in 0..count {
        let entry = ico.get(6 + index * 16..22 + index * 16)?;
        if entry[0] as usize != EDGE {
            continue;
        }
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        let header = ico.get(offset..offset + 20)?;
        if u32::from_le_bytes(header[0..4].try_into().ok()?) != 40
            || u32::from_le_bytes(header[8..12].try_into().ok()?) != (EDGE * 2) as u32
            || u16::from_le_bytes(header[14..16].try_into().ok()?) != 32
            || u32::from_le_bytes(header[16..20].try_into().ok()?) != 0
        {
            continue;
        }
        let pixels = ico.get(offset + 40..offset + 40 + EDGE * EDGE * 4)?;
        let mut rgba = vec![0u8; EDGE * EDGE * 4];
        for row in 0..EDGE {
            for col in 0..EDGE {
                let from = (EDGE - 1 - row) * EDGE * 4 + col * 4;
                let to = (row * EDGE + col) * 4;
                rgba[to] = pixels[from + 2];
                rgba[to + 1] = pixels[from + 1];
                rgba[to + 2] = pixels[from];
                rgba[to + 3] = pixels[from + 3];
            }
        }
        return Some(egui::IconData {
            rgba,
            width: EDGE as u32,
            height: EDGE as u32,
        });
    }
    None
}

fn main() -> eframe::Result {
    // 机器名从 svn_manager 换成 repo-manager：先搬家再接管，下面才有人读配置与自启状态
    config::migrate_legacy_dir();
    autostart::adopt_legacy();
    // 界面程序没有控制台，异常信息写入配置文件同目录的 panic.log 便于排查
    let panic_log = config::config_file().with_file_name("panic.log");
    std::panic::set_hook(Box::new(move |info| {
        let text = format!(
            "[{}] {info}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log)
        {
            let _ = file.write_all(text.as_bytes());
        }
    }));
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1220.0, 820.0])
        .with_min_inner_size([900.0, 580.0])
        .with_title(APP_TITLE);
    // 不显式设置的话，标题栏和任务栏用的还是 eframe 自带的默认图标
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        centered: true,
        ..Default::default()
    };
    eframe::run_native(APP_TITLE, options, Box::new(|cc| Ok(Box::new(RepoApp::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一张自己造的 48×48：逻辑顶行红、底行蓝、中间绿。
    /// 按 ico 的存法写进去 —— 行自下而上、字节 BGRA，所以翻转或换序都会让上下两沿对调
    fn striped_48() -> Vec<u8> {
        const E: usize = 48;
        let mut px = vec![0u8; E * E * 4];
        for row in 0..E {
            let bgra = if row == 0 {
                [0, 0, 255, 255]
            } else if row == E - 1 {
                [255, 0, 0, 255]
            } else {
                [0, 255, 0, 255]
            };
            let stored = E - 1 - row;
            for col in 0..E {
                px[stored * E * 4 + col * 4..stored * E * 4 + col * 4 + 4].copy_from_slice(&bgra);
            }
        }
        px
    }

    /// 拼一个最小 ico 文件：目录项 + 若干张 (边长, 像素)，每张前面 40 字节 DIB 头
    fn ico_with(images: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut table = Vec::new();
        let mut offset = 6 + 16 * images.len();
        for (edge, px) in images {
            let size = 40 + px.len();
            table.push((*edge, size, offset));
            offset += size;
        }
        let mut out: Vec<u8> = vec![0, 0, 1, 0, 0, 0];
        out[4] = images.len() as u8;
        for (edge, size, at) in &table {
            out.extend_from_slice(&[*edge as u8, *edge as u8, 0, 0]);
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&32u16.to_le_bytes());
            out.extend_from_slice(&(*size as u32).to_le_bytes());
            out.extend_from_slice(&(*at as u32).to_le_bytes());
        }
        for (edge, px) in images {
            out.extend_from_slice(&40u32.to_le_bytes());
            out.extend_from_slice(&(*edge as u32).to_le_bytes());
            out.extend_from_slice(&((*edge * 2) as u32).to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&32u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(px.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0u8; 16]);
            out.extend_from_slice(px);
        }
        out
    }

    /// 解码器的正事：挑对 48 那张、行翻成自上而下、BGRA 换成 RGBA。
    /// 靠真实画稿上某个恰好是白色的像素来验太脆，换一版图就得改断言，所以喂自己造的图
    #[test]
    fn icon_decoder_picks_48_and_keeps_rows_and_channels() {
        let file = ico_with(&[(16, vec![255; 16 * 16 * 4]), (48, striped_48())]);
        let icon = icon_rgba(&file).expect("该从里面解出 48px 那张");
        assert_eq!((icon.width, icon.height), (48, 48), "前面那张 16px 该跳过去");
        let px = |x: usize, y: usize| &icon.rgba[(y * 48 + x) * 4..(y * 48 + x) * 4 + 4];
        assert_eq!(px(0, 0), &[255, 0, 0, 255], "顶行不是红的：行序翻了或 B/R 写反了");
        assert_eq!(px(47, 47), &[0, 0, 255, 255], "底行不是蓝的");
        assert_eq!(px(20, 20), &[0, 255, 0, 255]);
    }

    /// 没有能用的那张就得返回 None，让 eframe 用它自己的默认图标
    #[test]
    fn icon_decoder_needs_a_32bpp_48_entry() {
        assert!(icon_rgba(&ico_with(&[(16, vec![255; 16 * 16 * 4])])).is_none());
        assert!(icon_rgba(&[]).is_none());
    }

    /// 内嵌那张只查「解得出 48px、不是一张空图」，画成什么样跟着设计走
    #[test]
    fn embedded_window_icon_is_not_blank() {
        let icon = window_icon().expect("icon/repo-manager.ico 里应能解析出 48px 的图");
        assert_eq!((icon.width, icon.height), (48, 48));
        assert_eq!(icon.rgba.len(), 48 * 48 * 4);
        let opaque = icon.rgba.chunks(4).filter(|p| p[3] > 8).count();
        assert!(
            opaque * 10 >= 48 * 48,
            "图标几乎是空的：不透明只占 {} / {}",
            opaque,
            48 * 48
        );
    }

    /// 「解决冲突」窗口的行口径与重建：只有真冲突进表，不完整的另数一句；
    /// status 重读后合完的那行消失，还在冲突的那行保住他挑好的策略和勾。
    #[test]
    fn resolve_rows_follow_the_latest_status() {
        use crate::svn::{Accept, Item, StatusEntry};
        let entry = |name: &str, item: Item| StatusEntry {
            path: format!(r"D:\wc\{name}"),
            name: name.to_owned(),
            item,
            props: "none".to_owned(),
            checked: false,
        };
        // 同一个文件名给两种状态：筛错一批这条用例就红
        let mixed = vec![
            entry("clash.txt", Item::Conflict),
            entry("half.txt", Item::Conflict),
            entry("half.txt", Item::Incomplete),
            entry("mod.txt", Item::Modified),
        ];
        let mut panel = Resolve::from_entries(0, &mixed);
        assert_eq!(panel.rows.len(), 2, "只有冲突条目进窗口，修改与不完整都不该列");
        assert_eq!(panel.incomplete, 1, "不完整另数一句：它走「更新」，不归 resolve 管");
        assert!(panel.rows.iter().all(|row| row.checked), "开窗口就是要处理它们，默认全勾");
        assert_eq!(panel.rows[0].accept, Accept::Working, "默认策略谁都不丢");
        assert!(!panel.drops_local(), "没人选「丢弃本地改动」时不该亮危险确认");

        panel.rows[0].accept = Accept::TheirsFull;
        panel.rows[0].checked = false;
        panel.rebuild(&[entry("clash.txt", Item::Conflict), entry("mod.txt", Item::Modified)]);
        assert_eq!(panel.rows.len(), 1, "他在 BC 里合完了的那行该从表上消失");
        assert_eq!(panel.rows[0].name, "clash.txt");
        assert_eq!(panel.rows[0].accept, Accept::TheirsFull, "重读不该把他挑好的策略抹平");
        assert!(!panel.rows[0].checked, "勾法同样要保住");
        assert_eq!(panel.incomplete, 0, "不完整的数跟着新读的 status 走");

        panel.rows[0].accept = Accept::Revert;
        panel.rows[0].checked = true;
        assert!(panel.drops_local(), "勾着的行里出现「丢弃本地改动」得多问一句");
        panel.rows[0].checked = false;
        assert!(!panel.drops_local(), "没勾的行不该替他承担后果");
    }
}
