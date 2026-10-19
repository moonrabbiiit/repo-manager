use std::path::{Path, PathBuf};

use egui::{Align, Button, Color32, FontId, Frame, Layout, RichText, ScrollArea, TextEdit, Ui, Vec2};

use crate::jobs::{Data, Kind, Sink};
use crate::localdiff::Excludes;
use crate::repo::RepoKind;
use crate::svn::{Item, StatusEntry, Svn};
use crate::{highlight, ink, Level, Maintain, RepoApp};
use crate::ui::{AlignedRow, field_margin};

/// 「上传 / 提交」页面状态。
///
/// 这里不存任何「正在读取 / 正在提交」标志位：这类标志位一旦后台任务异常结束就再也清不掉，
/// 界面会一直停在「看着能点、点了没反应」的状态。是否忙一律现问 `Pool`（见 `commit_page`）。
#[derive(Clone)]
pub struct CommitPage {
    pub dir: usize,
    pub label: String,
    pub entries: Vec<StatusEntry>,
    pub message: String,
    pub error: String,
    pub done_ok: bool,
    pub filter: String,
    /// 「显示屏蔽的 N 项」开关：关掉时被屏蔽的条目连列表都不出现
    pub show_excluded: bool,
    /// 与 entries 等长的屏蔽掩码，每帧在 `commit_page` 里按本目录规则重算，
    /// 显示、计数、提交清单、批量勾选四个口径共用这一份
    pub excluded: Vec<bool>,
    pub picked: usize,
    pub diff: String,
    /// 当前这份差异是否忽略空白与换行（按正在看的那一项决定：XML 自动勾选，其余默认不勾）
    pub diff_ignore: bool,
    pub confirm_revert: bool,
    /// 还没更新到最新时的第二下确认：第一下只把后果摆出来，再点一次才真的提交
    pub confirm_outdated: bool,
}

impl CommitPage {
    pub fn new(dir: usize, label: String) -> Self {
        Self {
            dir,
            label,
            entries: Vec::new(),
            message: String::new(),
            error: String::new(),
            done_ok: false,
            filter: String::new(),
            show_excluded: false,
            excluded: Vec::new(),
            picked: usize::MAX,
            diff: String::new(),
            diff_ignore: false,
            confirm_revert: false,
            confirm_outdated: false,
        }
    }

    pub fn set_entries(&mut self, entries: Vec<StatusEntry>) {
        self.entries = entries;
        self.picked = usize::MAX;
        self.diff.clear();
        self.error.clear();
        // 屏蔽掩码是按条目索引存的下标对应关系，条目一换就全错位：清掉等下一帧重算
        self.excluded.clear();
        // 重新读过状态就把「没更新到最新」的确认撤掉：清单变了，之前那一下点的是旧清单
        self.confirm_outdated = false;
    }

    /// 按本目录的屏蔽规则重算掩码，一帧里在所有口径之前调用一次。
    pub(crate) fn apply_gate(&mut self, root: &Path, raw: &str) {
        let gate = Gate::new(root, raw);
        if gate.excludes.is_none() {
            self.excluded.clear();
            return;
        }
        self.excluded = self.entries.iter().map(|entry| gate.hits(entry)).collect();
    }

    /// 第 index 条是否被屏蔽。掩码越界一律按「没屏蔽」算：宁可多列一项让人看见，
    /// 也不能把该提交的东西静悄悄漏掉。
    pub(crate) fn is_excluded(&self, index: usize) -> bool {
        self.excluded.get(index).copied().unwrap_or(false)
    }

    /// 本目录当前被屏蔽掉的条目数（顶栏那颗开关上的数字）。
    pub(crate) fn hidden_count(&self) -> usize {
        self.excluded.iter().filter(|hidden| **hidden).count()
    }

    /// 三个批量勾选按钮共用：被屏蔽的条目一律跳过。勾不上的东西不该能被勾上，
    /// 否则又回到「屏幕上找不到、却照样传了上去」。
    fn set_checks(&mut self, keep: impl Fn(Item) -> bool) {
        for index in 0..self.entries.len() {
            if self.is_excluded(index) {
                continue;
            }
            self.entries[index].checked = keep(self.entries[index].item);
        }
    }

    /// 这次要提交的路径。未版本化(?) 与已丢失(!) 也算在内：提交前会先替它们补上
    /// svn add / svn delete，然后和修改项一起提交；冲突、不完整的一律不进来。
    pub fn commit_targets(&self) -> Vec<String> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                !self.is_excluded(*index)
                    && entry.checked
                    && (entry.item.committable()
                        || entry.item.needs_add()
                        || entry.item.needs_delete())
            })
            .map(|(_, entry)| entry.path.clone())
            .collect()
    }

    /// 勾选出来、提交前需要先执行 svn add / svn delete 的条目。
    pub fn selected_targets(&self, op: Maintain) -> Vec<String> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| entry.checked && !self.is_excluded(*index))
            .filter(|(_, entry)| match op {
                Maintain::Add => entry.item == Item::Unversioned,
                Maintain::Delete => entry.item == Item::Missing,
                _ => false,
            })
            .map(|(_, entry)| entry.path.clone())
            .collect()
    }

    /// 未版本化算进「新增」、已丢失算进「删除」（提交时会自动补 add / delete），
    /// 「需处理」只剩冲突、不完整这类必须人工干预的条目。被屏蔽的不参与计数。
    fn counts(&self) -> (usize, usize, usize, usize) {
        let mut added = 0;
        let mut modified = 0;
        let mut deleted = 0;
        let mut blocked = 0;
        for index in 0..self.entries.len() {
            if self.is_excluded(index) {
                continue;
            }
            match self.entries[index].item {
                Item::Added | Item::Unversioned => added += 1,
                Item::Modified | Item::Replaced => modified += 1,
                Item::Deleted | Item::Missing => deleted += 1,
                item if item.blocked() => blocked += 1,
                _ => {}
            }
        }
        (added, modified, deleted, blocked)
    }

    fn visible(&self) -> Vec<usize> {
        let needles = filter_needles(&self.filter);
        (0..self.entries.len())
            .filter(|index| self.show_excluded || !self.is_excluded(*index))
            .filter(|index| {
                needles.is_empty()
                    || needles.iter().any(|needle| {
                        let entry = &self.entries[*index];
                        entry.name.to_lowercase().contains(needle)
                            || entry.path.to_lowercase().contains(needle)
                    })
            })
            .collect()
    }
}

/// 过滤框是单行的，多个关键字用 `;` 隔开，任一命中即留下（OR）；空段丢掉、整体转小写。
/// 转好的列表同时喂给判定与高亮，两边才是同一个口径——以前判定用 Unicode 折叠、高亮只认
/// ASCII 大小写，中文关键字能过滤出来却标不亮。
pub(crate) fn filter_needles(raw: &str) -> Vec<String> {
    raw.split(';')
        .map(str::trim)
        .filter(|needle| !needle.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// 提交前要先补做标记的条目怎么说：计数为 0 的那一类不提，两类都没有时返回空串。
fn staging_note(adds: usize, deletes: usize, sep: &str) -> String {
    let mut parts = Vec::new();
    if adds > 0 {
        parts.push(format!("提交时新增 {adds} 项"));
    }
    if deletes > 0 {
        parts.push(format!("提交时删除 {deletes} 项"));
    }
    parts.join(sep)
}

/// 待提交列表展示用的相对路径：把绝对路径里的工作副本根前缀去掉，
/// 只留 `src\views\foo.vue` 这样的部分。路径不在根下（理论上不会发生）就原样返回。
fn relative_to_root(full: &str, root: &Path) -> String {
    let mut root = root.display().to_string();
    while root.ends_with(['\\', '/']) {
        root.pop();
    }
    let mut prefix = root;
    prefix.push(std::path::MAIN_SEPARATOR);
    if let Some(rest) = full.strip_prefix(&prefix) {
        if !rest.is_empty() {
            return rest.to_owned();
        }
    }
    full.to_owned()
}

/// 弹窗里那一框是「一行一条」，而 `Excludes::parse` 认的是 BC 那套 `;` 分隔、带模式前缀的写法。
/// 这里把两种写法缝合起来，让框里只写 `bin\`、`*.log` 就能生效，不必再抄一遍前缀：
/// 逐行去空白、丢空行与 `#` 注释与另一种模式的行（包含与排除各存各的，混进来只会让人以为能写），
/// 缺 `sign` 的补上；不带 `.\` / `\` 锚定却带层级的裸路径（`A\B\target\`）补成 `./A\B\target\`，
/// 因为这种写法在 `Excludes` 里根本打不中——issue #4 就是栽在这条上。`;` 分隔的老写法照样认。
fn normalize_rules(raw: &str, sign: char) -> String {
    let opposite = if sign == '-' { '+' } else { '-' };
    let mut parts = Vec::new();
    for line in raw.split(['\n', '\r', ';']).map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(opposite) {
            continue;
        }
        let body = line.strip_prefix(sign).unwrap_or(line).trim();
        if body.is_empty() {
            continue;
        }
        let anchored = body.starts_with(".\\")
            || body.starts_with("./")
            || body.starts_with('\\')
            || body.starts_with('/');
        let segments = body.split(['\\', '/']).filter(|s| !s.is_empty()).count();
        // 只有一段（`bin\`、`debug.log`）不用锚定，按末级名在任意层级命中正是想要的；
        // 多段又不锚定的必须补 `./`，否则整条规则形同没写
        let pattern = if anchored || segments < 2 {
            body.to_owned()
        } else {
            format!("./{body}")
        };
        parts.push(format!("{sign}{pattern}"));
    }
    parts.join(";")
}

/// 「屏蔽可提交项」与「不参与合并的文件」两格共用这套排除侧缝合规则：一行一条拼成 BC 的分号串、
/// 补 `-` 前缀、丢掉 `+` 行
pub(crate) fn normalize_excludes(raw: &str) -> String {
    normalize_rules(raw, '-')
}

/// 「参与合并的文件」白名单走的是同一套缝合规则，只是前缀换成 `+`、`-` 行丢掉：
/// 要挡东西是下面那格「不参与合并的文件」的职责，两格各管一边才说得清。
pub(crate) fn normalize_includes(raw: &str) -> String {
    normalize_rules(raw, '+')
}

/// `bc_filter` 在配置里是 BC 那套分号串（`-*.iml;-bin\`），弹窗里要一行一条：
/// 按分号与换行拆开、去空白、丢空行，用换行拼回去。
pub(crate) fn bc_filter_lines(raw: &str) -> String {
    raw.split([';', '\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 行视图拼回配置里的分号串。只做拆行与 `;` 拼接，不补 `-` 也不补 `./`：
/// 这份口径要和他用同一份筛选打开的 Beyond Compare 对得上，目录行那个单行编辑器
/// 写的也是原样文本，这里擅自加工反而会多出两个说不清的版本。
pub(crate) fn bc_filter_rules(buf: &str) -> String {
    buf.split(['\n', '\r', ';'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(";")
}

/// 一个目录的「屏蔽闸门」：把配置里那串规则解析一次，之后显示、计数、提交清单、提交前复核
/// 都反复问同一份。规则为空时压根不解析，默认用户零成本。
pub(crate) struct Gate {
    root: PathBuf,
    excludes: Option<Excludes>,
}

impl Gate {
    pub(crate) fn new(root: &Path, raw: &str) -> Self {
        let normalized = normalize_excludes(raw);
        Self {
            root: root.to_path_buf(),
            excludes: (!normalized.is_empty()).then(|| Excludes::parse(&normalized)),
        }
    }

    /// 一条本地绝对路径是否被屏蔽。只用本地 path，不碰 svn 的 url：
    /// url 是正斜杠的仓库地址，跟本地排除规则不是一个坐标系。
    pub(crate) fn hits_path(&self, full: &str) -> bool {
        self.excludes
            .as_ref()
            .map(|excludes| excludes.hits(&relative_to_root(full, &self.root)))
            .unwrap_or(false)
    }

    pub(crate) fn hits(&self, entry: &StatusEntry) -> bool {
        self.hits_path(&entry.path)
    }
}

/// 「真正会进提交清单的条目」——主页的待提交数、「全部上传」的清单、提交页的清单都必须走这一个
/// 口径，否则就出现「屏幕上看不见却照样传上去了」。
pub(crate) fn pending_entries(entries: &[StatusEntry], root: &Path, raw: &str) -> Vec<StatusEntry> {
    let gate = Gate::new(root, raw);
    entries
        .iter()
        .filter(|entry| entry.item.uploadable() && !gate.hits(entry))
        .cloned()
        .collect()
}

/// 这份清单里被屏蔽规则命中了多少条。口径与提交页顶栏「显示屏蔽的 N 项」完全一致（只问命中，
/// 不再筛掉冲突一类条目），两处数字不一样会让人以为规则在乱跳。
/// 规则窗口边写边报这个数，靠的是最近一次读取的完整清单，不必先进提交页。
pub(crate) fn excluded_count(entries: &[StatusEntry], root: &Path, raw: &str) -> usize {
    let gate = Gate::new(root, raw);
    entries.iter().filter(|entry| gate.hits(entry)).count()
}

/// 「全部上传」的三份名单：targets 交给 svn commit，adds / deletes 是提交前要先补的标记。
/// 单独成一个函数只为一件事——用例能拿同一批条目、同一个根、同一条规则，逐项比对它和提交页
/// 给出的清单，旁路各算一套正是这个功能最容易出的事故。
pub(crate) fn upload_plan(entries: &[StatusEntry], root: &Path, raw: &str) -> CommitPlan {
    let picked = pending_entries(entries, root, raw);
    let paths = |keep: fn(Item) -> bool| {
        picked
            .iter()
            .filter(|entry| keep(entry.item))
            .map(|entry| entry.path.clone())
            .collect()
    };
    CommitPlan {
        targets: picked.iter().map(|entry| entry.path.clone()).collect(),
        adds: paths(Item::needs_add),
        deletes: paths(Item::needs_delete),
        excludes: raw.to_owned(),
    }
}

/// 一次提交的清单：真正交给 `svn commit` 的路径，以及提交前要先补 svn add / svn delete 的路径。
/// `excludes` 是该目录的屏蔽规则原文：后台任务在提交前用它再复核一遍，旁路进来的清单（「全部上传」、
/// 界面还没来得及重读的旧勾选）也挡得住。
pub struct CommitPlan {
    pub targets: Vec<String>,
    pub adds: Vec<String>,
    pub deletes: Vec<String>,
    pub excludes: String,
}

/// 提交前的最后一道闸：把一份路径清单里被屏蔽规则命中的摘掉，返回摘掉的条数。
fn gate_list(list: &mut Vec<String>, gate: &Gate) -> usize {
    let before = list.len();
    list.retain(|full| !gate.hits_path(full));
    before - list.len()
}

impl RepoApp {
    /// 上传前「还没更新到最新」的提醒文案：最近一次检测读到服务器上还有没拉到本地的新改动
    /// （`svn status -u` 的口径，见 `DirView::out_of_date`）时给一句，否则 `None`。
    /// 还没读到（断网、刚启动还没检测）就不给——无从判断，宁可不说，也不能把「不知道」当「未更新」。
    pub fn not_up_to_date_hint(&self, index: usize) -> Option<String> {
        let view = self.dirs.get(index)?;
        let pending = view.out_of_date.filter(|count| *count > 0)?;
        if self.dir_kind(index) == RepoKind::Git {
            // git 的落后数是提交个数：远端有新提交时直接 push 会被拒，先更新再提交
            return Some(format!("未更新到最新：远端还有 {pending} 个提交没拉到本地"));
        }
        let revision = match view.remote_rev.as_str() {
            "" => String::new(),
            rev => format!("，远端已是 r{rev}"),
        };
        Some(format!(
            "未更新到最新版本{revision}：服务器还有 {pending} 项改动没拉到本地"
        ))
    }

    /// 提交。勾选里的未版本化 / 已丢失条目会先补上 svn add / svn delete，
    /// 再和修改项一起 svn commit；add / delete 只成一部分时那几条会被跳过并写明原因。
    /// Git 目录：`git add -A` + `git commit` + 自动 push（提交即上传，见 `git_commit_run`）。
    pub fn spawn_commit(&mut self, index: usize, plan: CommitPlan, message: String) {
        let Some(path) = self.dir_path(index) else {
            self.hint("无法提交：该目录已从列表中移除");
            return;
        };
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            if plan.targets.is_empty() {
                self.hint("请先勾选要提交的文件（新增、修改、删除项可一并提交）");
                return;
            }
            // git 不收空的提交说明（svn 可以留空，git 会直接拒掉）
            if message.trim().is_empty() {
                self.hint("Git 提交需要提交说明：请先在「提交备注」中填写本次修改内容");
                return;
            }
            if self.pool.has(Kind::Commit, index) {
                self.hint("该目录的提交任务还在执行中，请等当前提交结束");
                return;
            }
            let label = self.dir_label(index);
            let count = plan.targets.len();
            if let Some(commit) = self.commit.as_mut() {
                commit.done_ok = false;
            }
            self.pool
                .spawn(Kind::Commit, index, format!("{label} 提交 {count} 项"), move |sink| {
                    git_commit_run(&git, &sink, &path, &label, index, plan.targets, &message)
                });
            return;
        }
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            self.hint("无法提交：svn.exe 不可用，或该目录已从列表中移除");
            return;
        };
        if plan.targets.is_empty() {
            self.hint("请先勾选要提交的文件（新增、修改、删除项可一并提交）");
            return;
        }
        if self.pool.has(Kind::Commit, index) {
            self.hint("该目录的提交任务还在执行中，请等当前提交结束");
            return;
        }
        let label = self.dir_label(index);
        let count = plan.targets.len();
        let update_after = self.cfg.update_after_commit;
        if let Some(commit) = self.commit.as_mut() {
            commit.done_ok = false;
        }
        self.pool
            .spawn(Kind::Commit, index, format!("{label} 提交 {count} 项"), move |sink| {
                commit_run(&svn, &sink, &path, &label, index, plan, &message, update_after)
            });
    }

    /// 「全部上传」：不看勾选，把该目录里所有能提交的条目一次提交上去。
    /// 清单在后台现读状态，口径和提交页的「只选可提交项」一致：未版本化 / 已丢失
    /// 一并带上（svn 先补 add / delete；git 的 add -A 天生就管），没有可提交改动时跳过。
    pub fn spawn_upload_all(&mut self, index: usize, message: String) {
        let Some(path) = self.dir_path(index) else {
            self.hint("无法提交：该目录已从列表中移除");
            return;
        };
        let excludes = self
            .cfg
            .dirs
            .get(index)
            .map(|dir| dir.commit_excludes.clone())
            .unwrap_or_default();
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            // git 不收空的提交说明（svn 可以留空，git 会直接拒掉）
            if message.trim().is_empty() {
                self.hint("Git 提交需要提交说明：请先填写提交说明");
                return;
            }
            if self.pool.has(Kind::Commit, index) {
                self.hint("该目录的提交任务还在执行中，请等当前提交结束");
                return;
            }
            let label = self.dir_label(index);
            self.pool
                .spawn(Kind::Commit, index, format!("{label} 全部上传"), move |sink| {
                    let (entries, stat) = git.status(&path);
                    if !stat.ok {
                        return Data::Run {
                            dir: index,
                            ok: false,
                            message: format!("{label}：读取本地修改失败——{}", stat.summary()),
                            reload: false,
                        };
                    }
                    let targets: Vec<String> = pending_entries(&entries, &path, &excludes)
                        .into_iter()
                        .map(|entry| entry.path)
                        .collect();
                    if targets.is_empty() {
                        return Data::Run {
                            dir: index,
                            ok: true,
                            message: format!("{label}：没有可提交的改动，已跳过"),
                            reload: false,
                        };
                    }
                    sink.line(format!("{label}：全部上传 {} 项", targets.len()));
                    git_commit_run(&git, &sink, &path, &label, index, targets, &message)
                });
            return;
        }
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            self.hint("无法提交：svn.exe 不可用，或该目录已从列表中移除");
            return;
        };
        if self.pool.has(Kind::Commit, index) {
            self.hint("该目录的提交任务还在执行中，请等当前提交结束");
            return;
        }
        let label = self.dir_label(index);
        let update_after = self.cfg.update_after_commit;
        self.pool
            .spawn(Kind::Commit, index, format!("{label} 全部上传"), move |sink| {
                let (entries, stat) = svn.status(&path);
                if !stat.ok {
                    let text = format!("{label}：读取本地修改失败——{}", stat.summary());
                    return Data::Run {
                        dir: index,
                        ok: false,
                        message: text,
                        reload: false,
                    };
                }
                let plan = upload_plan(&entries, &path, &excludes);
                if plan.targets.is_empty() {
                    let text = format!("{label}：没有可提交的改动，已跳过");
                    return Data::Run {
                        dir: index,
                        ok: true,
                        message: text,
                        reload: false,
                    };
                }
                let counts = staging_note(plan.adds.len(), plan.deletes.len(), "、");
                let breakdown = if counts.is_empty() {
                    String::new()
                } else {
                    format!("（其中{counts}）")
                };
                sink.line(format!(
                    "{label}：全部上传 {} 项{breakdown}",
                    plan.targets.len()
                ));
                commit_run(&svn, &sink, &path, &label, index, plan, &message, update_after)
            });
    }

    pub fn commit_page(&mut self, ui: &mut Ui) {
        let mut page = match self.commit.clone() {
            Some(page) => page,
            None => {
                self.back_to_main();
                return;
            }
        };
        let Some(path) = self.dir_path(page.dir) else {
            self.back_to_main();
            return;
        };
        // 屏蔽掩码一帧只算一次，后面显示 / 计数 / 清单 / 批量勾选四个口径共用这一份
        let excludes = self
            .cfg
            .dirs
            .get(page.dir)
            .map(|dir| dir.commit_excludes.clone())
            .unwrap_or_default();
        page.apply_gate(&path, &excludes);
        let git_kind = self.dir_kind(page.dir) == RepoKind::Git;
        let url = self
            .dirs
            .get(page.dir)
            .and_then(|view| view.info.as_ref())
            .map(|info| info.url.clone())
            .unwrap_or_default();
        // 是否忙一律由任务池现算：写类任务挡住「提交 / add / delete / revert」，
        // 读类任务（status、diff）只用于显示转圈，绝不影响按钮能否点中
        let writing = self.pool.is_writing(page.dir);
        let reading = self.pool.has(Kind::Status, page.dir);
        let diffing = self.pool.has(Kind::Diff, page.dir);

        ui.aligned_row(|ui| {
            if ui.button("← 返回目录").clicked() {
                self.back_to_main();
                self.commit = None;
                return;
            }
            ui.label(RichText::new(format!("上传 / 提交：{}", page.label)).size(17.0).strong());
            ui.label(RichText::new(path.display().to_string()).size(12.0).weak());
            // 仓库地址跟在本地路径后面，同一行放得下就一行说完
            if !url.is_empty() {
                ui.label(
                    RichText::new(format!("→ {url}"))
                        .size(12.0)
                        .color(ink(ui, Color32::from_rgb(140, 205, 255))),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if writing || reading || diffing {
                    ui.spinner();
                }
                if ui.button("重新读取修改").clicked() {
                    page.error.clear();
                    self.spawn_status(page.dir, false);
                }
            });
        });
        // 上传前先亮出基线：服务器上还有没拉到本地的新改动时，这次提交是在旧基线上改出来的，
        // 别人也动过同一处就会撞出冲突。点这一句直接跑该目录的 svn update；有写操作在执行时
        // 不给点——避免更新和提交同时改一个工作副本。
        if let Some(warning) = self.not_up_to_date_hint(page.dir) {
            let hit = ui.add(
                egui::Label::new(
                    RichText::new(&warning)
                        .size(12.5)
                        .strong()
                        .underline()
                        .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                )
                .sense(egui::Sense::click()),
            );
            if hit.clicked() && !writing {
                // 去更新就等于放弃了「没更新也提交」的确认，回来要重新点两下
                page.confirm_outdated = false;
                self.hint(format!("{}：正在更新到最新 …", page.label));
                self.spawn_update(page.dir);
            }
            hit.on_hover_text(if writing {
                "服务器上还有没拉到本地的新改动；该目录正有写操作在执行，等它结束再更新".to_owned()
            } else if git_kind {
                format!("服务器上还有没拉到本地的新改动，这次上传是在旧基线上做的；点这一句直接执行该目录的 git pull")
            } else {
                format!("服务器上还有没拉到本地的新改动，这次上传是在旧基线上做的；点这一句直接执行该目录的 svn update")
            });
        }
        ui.separator();

        let (added, modified, deleted, blocked) = page.counts();
        // 「共 N 项」跟旁边三项同一口径：被屏蔽的不算在本次提交里，也就不算进这个数
        let total = page.entries.len() - page.hidden_count();
        ui.aligned_row(|ui| {
            // 统计和操作按钮合并成一行：左边是口径，右边是动作
            ui.label(RichText::new(format!("本次修改内容：共 {total} 项")).strong().size(13.5));
            ui.label(
                RichText::new(format!("新增 {added}"))
                    .color(ink(ui, Color32::from_rgb(60, 190, 110)))
                    .size(12.5),
            );
            ui.label(
                RichText::new(format!("修改 {modified}"))
                    .color(ink(ui, Color32::from_rgb(90, 160, 240)))
                    .size(12.5),
            );
            ui.label(
                RichText::new(format!("删除 {deleted}"))
                    .color(ink(ui, Color32::from_rgb(235, 90, 90)))
                    .size(12.5),
            );
            if blocked > 0 {
                ui.label(
                    RichText::new(format!("需处理 {blocked}"))
                        .color(ink(ui, Color32::from_rgb(240, 190, 70)))
                        .size(12.5),
                );
            }
            ui.label(RichText::new("|").weak());
            // 「全选」是字面意义全勾（含冲突）；计数只认能提交的条目，冲突等特殊状态
            // 勾上了也不会进提交——默认进场就是按 uploadable 勾好的，一般用不着它
            if ui
                .button("全选")
                .on_hover_text("勾选列表中的所有条目（本目录已屏蔽的条目不参与）。注意：冲突等须先人工处理的状态，即使勾选也不会被提交；如需恢复默认勾选，请点击「只选可提交项」")
                .clicked()
            {
                page.set_checks(|_| true);
            }
            if ui.button("全不选").clicked() {
                page.set_checks(|_| false);
            }
            if ui
                .button("只选可提交项")
                .on_hover_text("恢复默认勾选：勾选所有可随本次上传提交的条目（含提交时新增的未版本化项、提交时删除的已丢失项），冲突等须先人工处理的条目不勾选；本目录已屏蔽的条目不参与")
                .clicked()
            {
                page.set_checks(Item::uploadable);
            }
            ui.label(RichText::new("|").weak());
            // 会改动工作副本的按钮：有写任务时直接置灰（置灰即点不动，显示与行为不会打架）
            ui.add_enabled_ui(!writing, |ui| {
                // 这两个按钮只是「提前单独执行一次」，不点也行：提交时会自动补上 add / delete
                let mut add = ui
                    .button("加入版本控制")
                    .on_hover_text("对勾选的未版本化文件执行 svn add（提交时会自动执行，无需先点这里）");
                if !page.entries.iter().any(|entry| entry.item.needs_add()) {
                    add = add.on_hover_text("没有未版本化条目");
                }
                if add.clicked() {
                    self.spawn_maintain(page.dir, Maintain::Add);
                }
                let mut remove = ui
                    .button("标记删除")
                    .on_hover_text("对勾选的丢失文件执行 svn delete（提交时会自动执行，无需先点这里）");
                if !page.entries.iter().any(|entry| entry.item.needs_delete()) {
                    remove = remove.on_hover_text("没有已丢失条目");
                }
                if remove.clicked() {
                    self.spawn_maintain(page.dir, Maintain::Delete);
                }
                // 屏蔽挡得住提交，挡不住 `svn revert --recursive`：整目录一起倒回去。
                // 唯一的例外，所以两态都要在 hover 里说清有多少被屏蔽的项会跟着丢
                let hidden = page.hidden_count();
                let revert_tip = if hidden > 0 {
                    format!(
                        "svn revert --recursive：本目录已屏蔽的 {hidden} 项也会被一起撤销，改动不可恢复"
                    )
                } else {
                    "svn revert --recursive".to_owned()
                };
                let revert = if page.confirm_revert {
                    ui.add(
                        Button::new(
                            RichText::new("确认撤销全部修改？")
                                .strong()
                                .color(Color32::from_rgb(255, 240, 240)),
                        )
                        .fill(Color32::from_rgb(150, 60, 60)),
                    )
                } else {
                    ui.button("撤销全部修改")
                };
                let revert = revert.on_hover_text(revert_tip);
                if revert.clicked() {
                    if page.confirm_revert {
                        page.confirm_revert = false;
                        self.spawn_maintain(page.dir, Maintain::Revert);
                    } else {
                        page.confirm_revert = true;
                        self.hint(if hidden > 0 {
                            format!(
                                "再次点击将丢弃该目录下所有本地修改（不可恢复），含已屏蔽的 {hidden} 项"
                            )
                        } else {
                            "再次点击将丢弃该目录下所有本地修改（不可恢复）".to_owned()
                        });
                    }
                }
                if page.confirm_revert && ui.button("放弃撤销").clicked() {
                    page.confirm_revert = false;
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(
                    TextEdit::singleline(&mut page.filter)
                        .hint_text("按名称过滤（命中处高亮）")
                        .margin(field_margin(ui))
                        .desired_width(210.0),
                )
                .on_hover_text("只是这一页看不到了，勾着的照样会提交：要长期不提交某类文件，写进目录行「更多 → 差异过滤与提交屏蔽」。多个关键字用 ; 隔开");
                // 一条都没屏蔽时不必占一个开关位；N 就是这枚开关的存在理由，写进标签里
                let hidden = page.hidden_count();
                if hidden > 0 {
                    ui.toggle_value(&mut page.show_excluded, format!("显示屏蔽的 {hidden} 项"))
                        .on_hover_text(
                            "勾上就把被屏蔽的条目也列出来（置灰、勾不动）；它们不会进本次提交清单，规则在目录行「更多 → 差异过滤与提交屏蔽」里改",
                        );
                }
            });
        });
        if !page.error.is_empty() {
            ui.label(
                RichText::new(format!("读取失败：{}", page.error))
                    .color(ink(ui, Color32::from_rgb(240, 100, 100))),
            );
        }
        if page.done_ok {
            ui.label(
                RichText::new("✓ 提交成功，已重新读取修改列表")
                    .color(ink(ui, Color32::from_rgb(80, 200, 120))),
            );
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("提交备注").strong());
            let note = if git_kind {
                "必填：git 提交不允许空说明"
            } else {
                "可留空；不会自动带入任何内容"
            };
            ui.label(RichText::new(note).weak().size(11.5));
        });
        let targets = page.commit_targets();
        let picked_count = targets.len();
        // 勾选里的未版本化 / 已丢失条目：提交时先自动 svn add / svn delete，再和修改一起提交
        let adds = page.selected_targets(Maintain::Add);
        let deletes = page.selected_targets(Maintain::Delete);
        // 备注框和右边的提交按钮共用这一个高度，两边都按它 sizing，底部才对得齐
        let note_height = 60.0;
        // 「提交并关闭」点中后要离开提交页：闭包里 return 出不去，先记下来在外面收尾
        let mut close_page = false;
        ui.aligned_row(|ui| {
            let width = (ui.available_width() - 140.0).max(240.0);
            // 计数说明跟在备注框下面：右侧按钮列只有 140px 宽，这句会被挤成两行
            let staging_label = staging_note(adds.len(), deletes.len(), " · ");
            ui.vertical(|ui| {
                ui.add_sized(
                    Vec2::new(width, note_height),
                    TextEdit::multiline(&mut page.message)
                        .desired_width(width)
                        .desired_rows(3)
                        .hint_text("填写本次提交说明（可留空）"),
                );
                if !staging_label.is_empty() {
                    ui.label(
                        RichText::new(staging_label)
                            .color(ink(ui, Color32::from_rgb(240, 190, 70)))
                            .size(11.5),
                    )
                    .on_hover_text("勾选里的未版本化 / 已丢失条目，提交前会先加入版本控制或标记删除，和改动一起一次传上去");
                }
            });
            ui.with_layout(Layout::top_down(Align::Max), |ui| {
                // 配色、点击结果、按钮下方的说明文字全部来自这一个判断：
                // 不能提交时一定说明原因，能提交时点了就一定有动作
                let blocked = if writing {
                    Some("该目录有 SVN 写操作正在执行，请稍候")
                } else if picked_count == 0 {
                    Some("未勾选要提交的文件")
                } else {
                    None
                };
                // 没更新到最新就拦一下：第一下只把后果摆出来，再点一次才真的提交
                // （两步确认的写法同下面的「撤销全部修改」）
                let outdated = self.not_up_to_date_hint(page.dir);
                let armed = outdated.is_some() && page.confirm_outdated;
                // egui 0.36 的按钮不会根据底色自动调整文字颜色，浅色主题下需要自己同时指定底色与字色
                let (fill, text) = if armed && blocked.is_none() {
                    (
                        Color32::from_rgb(150, 60, 60),
                        Color32::from_rgb(255, 240, 240),
                    )
                } else {
                    match (ui.visuals().dark_mode, blocked.is_none()) {
                        (true, false) => (Color32::from_gray(70), Color32::from_gray(148)),
                        (true, true) => (
                            Color32::from_rgb(38, 112, 70),
                            Color32::from_rgb(228, 245, 235),
                        ),
                        (false, false) => (Color32::from_gray(214), Color32::from_gray(120)),
                        (false, true) => (
                            Color32::from_rgb(23, 102, 61),
                            Color32::from_rgb(255, 255, 255),
                        ),
                    }
                };
                let caption = if armed {
                    format!("仍要提交 {picked_count} 项？")
                } else {
                    format!("提交 {picked_count} 项")
                };
                let button =
                    egui::Button::new(RichText::new(caption).size(14.0).strong().color(text))
                        .fill(fill);
                // 两个提交按钮共用同一套拦截判断：Some(false) 提交后留在本页，Some(true) 提交完回目录页
                // 上下摞在一起，宽度对齐；「提交并关闭」不参与换色（armed 时主按钮已经够醒目）
                let mut action: Option<bool> = None;
                let response = ui
                    .add_sized(Vec2::new(126.0, 26.0), button)
                    .on_hover_text("提交后停留在本页，可继续查看结果");
                if response.clicked() {
                    action = Some(false);
                }
                if ui
                    .add_sized(
                        Vec2::new(126.0, 26.0),
                        egui::Button::new(RichText::new("提交并关闭").size(13.5)),
                    )
                    .on_hover_text("提交后直接返回目录页；任务在后台继续执行，可在任务区查看进度")
                    .clicked()
                {
                    action = Some(true);
                }
                if let Some(close_after) = action {
                    match blocked {
                        Some(why) => self.hint(why),
                        // 还没确认过就先拦住，只说明后果，不上传
                        None if outdated.is_some() && !page.confirm_outdated => {
                            page.confirm_outdated = true;
                            self.hint(format!(
                                "{}：再次点击「仍要提交」将按本地版本上传，也可先通过上方提示条更新到最新",
                                outdated.unwrap_or_default()
                            ));
                        }
                        None => {
                            let counts = staging_note(adds.len(), deletes.len(), "、");
                            let staging = if counts.is_empty() {
                                String::new()
                            } else {
                                format!("，其中{counts}")
                            };
                            // 没更新到最新还是让它传（用户可能就是要把手上这份传上去，而且已经确认过），
                            // 但要说清基线，并往输出记录写一条——提示条马上被下一句覆盖，只有记录留得住
                            match outdated.as_ref() {
                                Some(warning) => {
                                    self.push(
                                        Level::Warning,
                                        format!("{warning}，建议先「更新」再上传"),
                                    );
                                    self.hint(format!(
                                        "{warning}；仍按本地当前版本提交 {picked_count} 项 …"
                                    ));
                                }
                                None => self.hint(format!("正在提交 {picked_count} 项{staging} …")),
                            }
                            // 确认已经用掉了：下次进来要重新拦一遍（提交完的状态重读也会清掉它）
                            page.confirm_outdated = false;
                            self.spawn_commit(
                                page.dir,
                                CommitPlan {
                                    targets: targets.clone(),
                                    adds: adds.clone(),
                                    deletes: deletes.clone(),
                                    excludes: excludes.clone(),
                                },
                                page.message.clone(),
                            );
                            if close_after {
                                // 任务已在后台跑起来，这里直接回目录页；下面统一收尾
                                close_page = true;
                            }
                        }
                    }
                }
                if armed && blocked.is_none() {
                    // 拦住的这一下要给退路：要么先更新到最新，要么明确放弃
                    ui.aligned_row(|ui| {
                        let tool = if git_kind { "git pull" } else { "svn update" };
                        if ui
                            .button("先更新到最新")
                            .on_hover_text(format!("对该目录执行 {tool}，再重新读一遍本地改动"))
                            .clicked()
                        {
                            page.confirm_outdated = false;
                            self.hint(format!("{}：正在更新到最新 …", page.label));
                            self.spawn_update(page.dir);
                        }
                        if ui.button("放弃提交").clicked() {
                            page.confirm_outdated = false;
                        }
                    });
                }
                if let Some(why) = blocked {
                    ui.label(RichText::new(why).weak().size(11.5));
                }
            });
        });
        if close_page {
            // 提交并关闭：任务留在任务池里继续跑，页面直接回目录（不写回提交页状态）
            self.back_to_main();
            self.commit = None;
            return;
        }
        ui.separator();

        let needles = filter_needles(&page.filter);
        ui.columns(2, |columns| {
            let list_width = columns[0].available_width();
            let picked = page.picked;
            let visible = page.visible();
            if reading {
                columns[0].horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在读取 svn status …");
                });
            }
            ScrollArea::vertical()
                .id_salt("commit_list")
                .auto_shrink([false, false])
                .show(&mut columns[0], |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if visible.is_empty() && !reading {
                        // 三种「列表是空的」要说得开：被过滤掉了、改的全在屏蔽规则里、真没改
                        let empty_note = if !page.filter.trim().is_empty() {
                            "过滤后没有匹配项".to_owned()
                        } else if !page.entries.is_empty() {
                            format!("已屏蔽 {} 项，未列入本次提交", page.hidden_count())
                        } else {
                            "没有本地修改，无需提交".to_owned()
                        };
                        ui.label(RichText::new(empty_note).weak());
                    }
                    for index in visible {
                        let entry = page.entries[index].clone();
                        let hidden = page.is_excluded(index);
                        // A/M/D 标记与文件名共用同一个类型色；被屏蔽的整行压成弱色，
                        // 一眼看出这一条不在本次提交里（只有开了开关才会出现在这儿）
                        let rgb = entry.item.color();
                        let color = if hidden {
                            ui.visuals().weak_text_color()
                        } else {
                            ink(ui, Color32::from_rgb(rgb.0, rgb.1, rgb.2))
                        };
                        Frame::new()
                            .inner_margin(3.0)
                            .corner_radius(4.0)
                            .fill(if picked == index {
                                ui.visuals().selection.bg_fill
                            } else {
                                Color32::TRANSPARENT
                            })
                            .show(ui, |ui| {
                                ui.aligned_row(|ui| {
                                    let mut checked = entry.checked;
                                    // 屏蔽行连勾选都禁用：勾不动才和「不进提交清单」说得一致
                                    let toggle = ui
                                        .add_enabled_ui(!hidden, |ui| ui.checkbox(&mut checked, ""))
                                        .inner;
                                    if toggle.changed() {
                                        page.entries[index].checked = checked;
                                    }
                                    toggle.on_hover_text(if hidden {
                                        "已被本目录的屏蔽规则挡下，不会进入本次提交；规则写在目录行「更多 → 差异过滤与提交屏蔽」里"
                                    } else {
                                        "勾选后提交该项"
                                    });
                                    ui.label(
                                        RichText::new(entry.item.mark().to_string())
                                            .strong()
                                            .monospace()
                                            .size(13.0)
                                            .color(color),
                                    )
                                    .on_hover_text(entry.item.text());
                                    // 文件名跟着类型着色（新增绿 / 修改蓝 / 删除红 …）；
                                    // 列表展示相对路径（去掉工作副本根前缀），悬停可见完整路径。
                                    // 不能用 add_sized：它内部是居中布局，会把文字摆到格子中间；
                                    // 这里显式分配整格宽度并用左对齐布局，文字才真正贴左
                                    let display = relative_to_root(&entry.path, &path);
                                    let name = ui
                                        .allocate_ui_with_layout(
                                            Vec2::new((list_width - 130.0).max(90.0), 18.0),
                                            Layout::left_to_right(Align::Center),
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(highlight(
                                                        ui,
                                                        &display,
                                                        &needles,
                                                        FontId::proportional(12.5),
                                                        color,
                                                    ))
                                                    .truncate()
                                                    .sense(egui::Sense::click()),
                                                )
                                            },
                                        )
                                        .inner
                                        .on_hover_text(format!(
                                            "{}\n状态：{}",
                                            entry.path,
                                            entry.item.text()
                                        ));
                                    if name.clicked() {
                                        page.picked = index;
                                        page.diff.clear();
                                        // 点选文件即自动读取差异，不必再点「查看该项差异」
                                        if entry.item == Item::Unversioned || entry.item == Item::Ignored {
                                            page.diff = "该条目尚未加入版本控制，没有差异可比对。".to_owned();
                                        } else {
                                            page.diff = "正在读取差异 …".to_owned();
                                            // XML 被编辑器整体改掉缩进 / 换行符的情况最多，这类默认忽略空白
                                            page.diff_ignore =
                                                entry.path.to_ascii_lowercase().ends_with(".xml");
                                            let ignore = page.diff_ignore;
                                            self.spawn_diff(page.dir, entry.path.clone(), ignore);
                                        }
                                    }
                                    // 这两类不再需要先手动处理：勾上就会在提交时自动补 add / delete
                                    if entry.item.needs_add() {
                                        ui.label(
                                            RichText::new("提交时新增")
                                                .size(11.0)
                                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                        )
                                        .on_hover_text(
                                            "这个文件目前只在本地，勾选后提交时会先加入版本控制（svn add），再作为新文件上传到服务器",
                                        );
                                    } else if entry.item.needs_delete() {
                                        ui.label(
                                            RichText::new("提交时删除")
                                                .size(11.0)
                                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                        )
                                        .on_hover_text(
                                            "这个文件本地已经删掉了，勾选后提交时会先标记删除（svn delete），服务器上的那份也一并删除",
                                        );
                                    }
                                    if entry.props != "none" {
                                        ui.label(RichText::new("属性变更").size(11.0).weak());
                                    }
                                    if hidden {
                                        ui.label(RichText::new("已屏蔽").size(11.0).weak())
                                        .on_hover_text("本目录的屏蔽规则挡住了这一条：不显示、不勾选、不进提交清单；这一行是顶栏「显示屏蔽的 N 项」开关打开后才出现的");
                                    }
                                });
                            });
                    }
                });
            if picked != usize::MAX {
                let target = page.entries.get(picked).map(|e| (e.path.clone(), e.item));
                if let Some((target_path, item)) = target {
                    columns[0].add_enabled_ui(!(diffing || writing), |ui| {
                        ui.aligned_row(|ui| {
                            if ui.button("查看该项差异").clicked() {
                                if item == Item::Unversioned || item == Item::Ignored {
                                    page.diff = "该条目尚未加入版本控制，没有差异可比对。".to_owned();
                                } else {
                                    page.diff = "正在读取差异 …".to_owned();
                                    page.diff_ignore =
                                        target_path.to_ascii_lowercase().ends_with(".xml");
                                    let ignore = page.diff_ignore;
                                    self.spawn_diff(page.dir, target_path.clone(), ignore);
                                }
                            }
                            if ui
                                .button("Beyond Compare")
                                .on_hover_text("用 Beyond Compare 对比 BASE 版本与本地文件")
                                .clicked()
                            {
                                self.open_bcompare_file(page.dir, target_path.clone());
                            }
                        });
                    });
                    // 选中项的完整路径也跟着类型着色
                    let rgb = item.color();
                    let job = highlight(
                        &columns[0],
                        &target_path,
                        &needles,
                        FontId::proportional(11.0),
                        ink(&columns[0], Color32::from_rgb(rgb.0, rgb.1, rgb.2)),
                    );
                    columns[0].label(job);
                }
            }

            // ---- 右列：差异 / 说明 ----
            let ui2 = &mut columns[1];
            ui2.horizontal(|ui| {
                ui.label(RichText::new("差异 / 明细").strong());
                if !page.diff.is_empty() && ui.button("清空").clicked() {
                    page.diff.clear();
                }
                // 这个开关只管当前这一项：换文件看差异时会按扩展名重新决定默认值
                let mut ignore = page.diff_ignore;
                if ui
                    .checkbox(&mut ignore, "忽略空白与换行")
                    .on_hover_text(
                        "XML 文件默认勾选（此类文件的缩进与换行符常被编辑器整体改写（CRLF↔LF），\
                         不忽略时 svn 会将整份文件报告为改动，无法体现实际修改的行），其余文件默认不勾选。\n\
                         勾选后按 svn diff -x \"-w --ignore-eol-style\" 比较；取消勾选可查看原始差异。",
                    )
                    .changed()
                {
                    page.diff_ignore = ignore;
                    page.diff.clear();
                    if picked != usize::MAX {
                        if let Some(entry) = page.entries.get(picked) {
                            self.spawn_diff(page.dir, entry.path.clone(), ignore);
                        }
                    }
                }
                ui.label(RichText::new("点选左侧文件自动显示差异").weak().size(11.5));
            });
            ui2.separator();
            if diffing {
                ui2.horizontal(|ui| {
                    ui.spinner();
                    ui.label("svn diff …");
                });
            } else if page.diff.trim().is_empty() {
                ui2.label(
                    RichText::new(
                        "提示：\n· 勾选要提交的文件，备注可留空，点「提交」即可\n· 新增 / 修改 / 删除可以一起提交：未版本化(?) 提交时先加入版本控制，\n  已丢失(!) 提交时先标记删除，再和修改项一次传上去\n· 冲突(C) 的条目要先解决，不会被带进提交",
                    )
                    .weak(),
                );
                if page.entries.is_empty() && !reading {
                    ui2.label(
                        RichText::new("该目录当前没有本地修改")
                            .color(ink(ui2, Color32::from_rgb(80, 200, 120))),
                    );
                }
            } else {
                ScrollArea::horizontal().id_salt("diff_x").show(ui2, |ui| {
                    ScrollArea::vertical()
                        .id_salt("diff_y")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for line in page.diff.lines() {
                                let color = if line.starts_with('+') && !line.starts_with("+++") {
                                    ink(ui, Color32::from_rgb(90, 210, 130))
                                } else if line.starts_with('-') && !line.starts_with("---") {
                                    ink(ui, Color32::from_rgb(235, 110, 110))
                                } else if line.starts_with("@@") || line.starts_with("Index:") {
                                    ink(ui, Color32::from_rgb(120, 190, 240))
                                } else {
                                    ui.visuals().text_color()
                                };
                                ui.monospace(RichText::new(line).size(12.0).color(color));
                            }
                        });
                });
            }
        });

        self.commit = Some(page);
    }
}

/// 提交的实际流程：add → delete → 复核状态 → commit（成功后按设置补一次 update）。
/// 「上传 / 提交」页的提交按钮和「全部上传」共用这一条，区别只是清单由谁凑出来。
fn commit_run(
    svn: &Svn,
    sink: &Sink,
    path: &Path,
    label: &str,
    index: usize,
    plan: CommitPlan,
    message: &str,
    update_after_commit: bool,
) -> Data {
    let CommitPlan { mut targets, mut adds, mut deletes, excludes } = plan;
    // 最后一道闸：清单里被本目录屏蔽规则命中的路径一律摘掉。提交页给的清单本来就在屏幕上挡好了，
    // 这一层防的是旁路——「全部上传」现读的清单、以及刚改完规则还没重新读取状态时的旧勾选
    let gate = Gate::new(path, &excludes);
    let dropped = gate_list(&mut targets, &gate)
        + gate_list(&mut adds, &gate)
        + gate_list(&mut deletes, &gate);
    if dropped > 0 {
        sink.line(format!("{label}：本目录屏蔽规则拦下 {dropped} 项，未提交"));
    }
    let count = targets.len();
    if !adds.is_empty() {
        sink.line(format!("$ svn add --force （{} 项）", adds.len()));
        let run = svn.add(path, &adds, &|line| sink.line(line));
        if !run.ok {
            sink.line(format!("{label}：svn add 没有全部成功——{}", run.summary()));
        }
    }
    if !deletes.is_empty() {
        sink.line(format!("$ svn delete （{} 项）", deletes.len()));
        let run = svn.delete(path, &deletes, &|line| sink.line(line));
        if !run.ok {
            sink.line(format!("{label}：svn delete 没有全部成功——{}", run.summary()));
        }
    }
    // add / delete 可能只成功了几条，而混进一条未版本化的路径会让整次提交失败，
    // 所以提交前重新读一遍状态，只把真正进入 A/M/D/R 调度的路径交给 svn commit。
    let (fresh, stat) = svn.status(path);
    let mut commit_list: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    if stat.ok {
        for want in &targets {
            if fresh
                .iter()
                .any(|entry| &entry.path == want && entry.item.committable())
            {
                commit_list.push(want.clone());
            } else {
                skipped.push(want.clone());
            }
        }
    } else {
        sink.line(format!(
            "{label}：提交前复核状态没读成功，按清单直接提交——{}",
            stat.summary()
        ));
        commit_list = targets;
    }
    // 整目录 add 时子项会跟着一起进入 A 调度，但勾选的只有目录那一行；
    // --depth empty 只提交目录本身（实测子项留在本地待提交），所以把子项一并带上。
    // delete 不用管：提交被删的目录，服务端会连带删掉整棵子树。
    for want in &adds {
        if !commit_list.iter().any(|got| got == want) {
            continue;
        }
        let prefix = format!("{}\\", want.trim_end_matches(['\\', '/']));
        for child in &fresh {
            if child.item.committable()
                && child.path.starts_with(&prefix)
                && !commit_list.contains(&child.path)
                // 子项是从 fresh 现读出来的，不在这儿挡一道就会跟着目录一起传上去
                && !gate.hits(child)
            {
                commit_list.push(child.path.clone());
            }
        }
    }
    for miss in &skipped {
        sink.line(format!("已跳过（未能加入版本控制或标记删除）：{miss}"));
    }
    if commit_list.is_empty() {
        let text = format!("{label}：{count} 项都没能进入提交调度，已放弃提交");
        return Data::Run {
            dir: index,
            ok: false,
            message: text,
            reload: true,
        };
    }
    sink.line(format!(
        "$ svn commit --depth empty （{} 项）说明：{}",
        commit_list.len(),
        match message.trim() {
            "" => "（空）".to_owned(),
            text => text.lines().next().unwrap_or("").trim().to_owned(),
        }
    ));
    let run = svn.commit(path, &commit_list, message, &|line| sink.line(line));
    let text = if run.ok {
        let mut done = format!("{label}：已提交 {} 项", commit_list.len());
        if !skipped.is_empty() {
            done.push_str(&format!("，另有 {} 项被跳过（见输出记录）", skipped.len()));
        }
        done
    } else {
        format!("{label}：提交失败——{}", run.summary())
    };
    // 收尾文案只交给任务回收端统一写进输出区；这边再 sink.line 一次就会显示两遍
    // 提交只把被提交路径的版本推进，工作副本根目录还停在旧版本上；补一次 update
    // 将整棵树的 BASE 推到 HEAD，列表里的「本地 r」才会立刻跟上新提交的版本。
    // 注意：update 会把别人已提交的改动一起拉到本地，所以做成了「设置 → 常规」里的开关。
    // 收尾性质，正常时不往输出区刷 svn update 的逐行日志；只有没更新成功才提示一句。
    if run.ok && update_after_commit {
        let update = svn.update(path, &|_| {});
        if !update.ok {
            sink.line(format!(
                "{label}：提交成功，但提交后的更新没完成——{}",
                update.summary()
            ));
        }
    }

    // 就算提交没成功，add / delete 已经改过工作副本了，列表一律要重读
    Data::Run {
        dir: index,
        ok: run.ok,
        message: text,
        reload: true,
    }
}

/// Git 版的提交流程：`git add -A` 暂存 → `git commit`（只提交点名的路径）→ `git push`。
/// 「Git 的提交 = 上传」：commit 落地后紧接着推远端；push 失败不算提交失败，
/// 但要把「改动已经 commit 在本地、只差推送」讲清楚，被拒时顺带指出该先更新。
fn git_commit_run(
    git: &crate::git::Git,
    sink: &Sink,
    dir: &Path,
    label: &str,
    index: usize,
    targets: Vec<String>,
    message: &str,
) -> Data {
    let count = targets.len();
    sink.line(format!(
        "$ git add -A && git commit --（{} 项，说明：{}）",
        count,
        match message.trim() {
            "" => "（空）".to_owned(),
            text => text.lines().next().unwrap_or("").trim().to_owned(),
        }
    ));
    let run = git.commit(dir, &targets, message, &|line| sink.line(line));
    if !run.ok {
        return Data::Run {
            dir: index,
            ok: false,
            message: format!("{label}：提交失败——{}", run.summary()),
            reload: true,
        };
    }
    sink.line("$ git push");
    let pushed = git.push(dir, &|line| sink.line(line));
    let text = if pushed.ok {
        format!("{label}：已提交 {count} 项并推送到远端")
    } else {
        let rejected = ["rejected", "non-fast-forward", "fetch first"]
            .iter()
            .any(|mark| pushed.err.contains(mark) || pushed.out.contains(mark));
        let mut text =
            format!("{label}：提交已在本地完成（{count} 项），但推送失败——{}", pushed.summary());
        if rejected {
            text.push_str("\n（远端有你本地没有的提交：先「更新」拉下来，核对后再提交一次）");
        }
        text
    };
    Data::Run {
        dir: index,
        ok: pushed.ok,
        message: text,
        reload: true,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::svn::StatusEntry;

    fn entry(item: Item, checked: bool) -> StatusEntry {
        StatusEntry {
            path: format!("D:\\wc\\{}.java", item.mark()),
            name: format!("{}.java", item.mark()),
            item,
            props: "none".to_owned(),
            checked,
        }
    }

    fn page_of(items: Vec<Item>, checked: &[Item]) -> CommitPage {
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(
            items
                .into_iter()
                .map(|item| entry(item, checked.contains(&item)))
                .collect(),
        );
        page
    }

    /// 新增(?)、已丢失(!) 和修改一样能进提交目标，冲突 / 不完整不行。
    #[test]
    fn checked_new_and_missing_join_the_commit() {
        let page = page_of(
            vec![
                Item::Modified,
                Item::Added,
                Item::Deleted,
                Item::Replaced,
                Item::Unversioned,
                Item::Missing,
                Item::Conflict,
                Item::Incomplete,
                Item::Normal,
            ],
            &[
                Item::Modified,
                Item::Added,
                Item::Deleted,
                Item::Replaced,
                Item::Unversioned,
                Item::Missing,
                Item::Conflict,
                Item::Incomplete,
                Item::Normal,
            ],
        );
        let targets = page.commit_targets();
        assert_eq!(targets.len(), 6, "{targets:?}");
        assert!(targets.iter().any(|path| path.ends_with("?.java")));
        assert!(targets.iter().any(|path| path.ends_with("!.java")));
        assert!(!targets.iter().any(|path| path.ends_with("C.java")));
        // 没勾选的一律不算
        let unchecked = page_of(vec![Item::Unversioned, Item::Missing], &[]);
        assert!(unchecked.commit_targets().is_empty());
    }

    /// 提交前要补的 add / delete 名单只收勾选过的对应条目。
    #[test]
    fn staging_lists_only_take_checked_entries() {
        let page = page_of(
            vec![Item::Unversioned, Item::Missing, Item::Modified, Item::Added, Item::Deleted],
            &[Item::Unversioned, Item::Missing, Item::Modified],
        );
        assert_eq!(page.selected_targets(Maintain::Add), vec![r"D:\wc\?.java"]);
        assert_eq!(page.selected_targets(Maintain::Delete), vec![r"D:\wc\!.java"]);
        assert!(page.selected_targets(Maintain::Revert).is_empty());
    }

    /// 计数为 0 的那一类不写出来，界面上不该出现「提交时删除 0 项」这种说法。
    #[test]
    fn staging_note_skips_the_zero_side() {
        assert_eq!(staging_note(3, 1, " · "), "提交时新增 3 项 · 提交时删除 1 项");
        assert_eq!(staging_note(4, 0, " · "), "提交时新增 4 项");
        assert_eq!(staging_note(0, 2, "、"), "提交时删除 2 项");
        assert_eq!(staging_note(0, 0, " · "), "");
    }

    /// 计数里未版本化算新增、已丢失算删除，「需处理」只剩冲突和不完整。
    #[test]
    fn counters_fold_pending_add_and_delete_into_totals() {
        let page = page_of(
            vec![
                Item::Added,
                Item::Unversioned,
                Item::Modified,
                Item::Deleted,
                Item::Missing,
                Item::Conflict,
            ],
            &[],
        );
        assert_eq!(page.counts(), (2, 1, 2, 1));
    }

    /// 列表展示相对路径：去掉工作副本根前缀，前缀之外的同名目录不能误伤。
    #[test]
    fn list_shows_paths_relative_to_working_copy_root() {
        assert_eq!(
            relative_to_root(r"D:\wc\src\views\index.vue", Path::new(r"D:\wc")),
            r"src\views\index.vue"
        );
        // 根路径尾部带不带分隔符都认
        assert_eq!(
            relative_to_root(r"D:\wc\a.vue", Path::new(r"D:\wc\")),
            r"a.vue"
        );
        // 根目录本身就是条目时保持原样
        assert_eq!(relative_to_root(r"D:\wc", Path::new(r"D:\wc")), r"D:\wc");
        // 只是共享前缀的兄弟目录不能被切掉
        assert_eq!(
            relative_to_root(r"D:\wc2\a.vue", Path::new(r"D:\wc")),
            r"D:\wc2\a.vue"
        );
    }

    /// 上传前的提醒只在「读到服务器上确实还有没拉到本地的新改动」时给：
    /// 没检测过（`None`，断网或刚启动）不猜，已是最新（`Some(0)`）也不说。
    #[test]
    fn not_up_to_date_hint_only_fires_on_known_pending_changes() {
        let mut app = crate::testbed::stub_app(false);
        // 测试台里 dir 0 = 服务器还有 5 项没拉下来，远端 r128
        let hint = app.not_up_to_date_hint(0).expect("有可更新项就要提示");
        assert!(hint.starts_with("未更新到最新版本"), "{hint}");
        assert!(hint.contains("r128") && hint.contains('5'), "{hint}");

        assert_eq!(app.not_up_to_date_hint(1), None, "已是最新就不提示");
        assert_eq!(app.not_up_to_date_hint(2), None, "还没读到过就不猜");
        assert_eq!(app.not_up_to_date_hint(99), None, "越界的目录不能 panic");

        // 远端版本号没读到时不带那半句，其余照旧
        app.dirs[0].remote_rev.clear();
        let hint = app.not_up_to_date_hint(0).expect("有可更新项就要提示");
        assert!(!hint.contains("远端已是"), "{hint}");
    }

    /// 提交页跑一帧，收下画出来的「文字 + 矩形」（同 testbed 里主页那套收法）。
    fn commit_frame(ctx: &egui::Context, app: &mut RepoApp) -> Vec<(String, egui::Rect)> {
        let out = ctx.run_ui(crate::testbed::base_input(), |ui| app.commit_page(ui));
        commit_drawn(out)
    }

    /// 在提交页上点一下（位置由上一帧的矩形给），再收一帧。
    fn commit_click(
        ctx: &egui::Context,
        app: &mut RepoApp,
        pos: egui::Pos2,
    ) -> Vec<(String, egui::Rect)> {
        let press = |pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut input = crate::testbed::base_input();
        input.events = vec![egui::Event::PointerMoved(pos), press(true), press(false)];
        let out = ctx.run_ui(input, |ui| app.commit_page(ui));
        commit_drawn(out)
    }

    fn commit_drawn(mut out: egui::FullOutput) -> Vec<(String, egui::Rect)> {
        // 测试里没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    item.shape.visual_bounding_rect(),
                )),
                _ => None,
            })
            .filter(|(text, _)| !text.trim().is_empty())
            .collect()
    }

    fn rect_of(drawn: &[(String, egui::Rect)], label: &str) -> egui::Rect {
        drawn
            .iter()
            .find(|(text, _)| text == label)
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("页面上没画出「{label}」"))
    }

    fn has(drawn: &[(String, egui::Rect)], label: &str) -> bool {
        drawn.iter().any(|(text, _)| text == label)
    }

    /// 断言失败时把这一帧画出来的文字整体打出来，省得靠猜页面上到底有什么
    fn texts_of(drawn: &[(String, egui::Rect)]) -> Vec<String> {
        drawn.iter().map(|(text, _)| text.clone()).collect()
    }

    /// 提交页连同帧末的「差异过滤与提交屏蔽」窗口一起跑一帧：弹窗挂在帧末，测试台那条路
    /// 只跑主页，所以这里必须自己补调一次，否则窗口在测试里永远画不出来。
    fn rules_frame(
        ctx: &egui::Context,
        app: &mut RepoApp,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Rect)> {
        let mut input = crate::testbed::base_input();
        input.events = events;
        let out = ctx.run_ui(input, |ui| {
            app.commit_page(ui);
            app.commit_rules_dialog(ui.ctx());
        });
        commit_drawn(out)
    }

    /// 按文字片段取矩形：多行框本身不是文字，拿它的占位示例定位才拿得到整块输入区。
    fn rect_containing(drawn: &[(String, egui::Rect)], needle: &str) -> egui::Rect {
        drawn
            .iter()
            .find(|(text, _)| text.contains(needle))
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("页面上没画出含「{needle}」的文字：{:?}", texts_of(drawn)))
    }

    fn pointer(events: &mut Vec<egui::Event>, pos: egui::Pos2) {
        let button = |pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        events.push(egui::Event::PointerMoved(pos));
        events.push(button(true));
        events.push(button(false));
    }

    /// 这句要在提交页一直亮着（点它直接更新），不是只在按下提交那一瞬闪一下。
    #[test]
    fn commit_page_shows_the_not_up_to_date_warning() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);

        app.commit = Some(page_of(vec![Item::Modified], &[Item::Modified]));
        let drawn = commit_frame(&ctx, &mut app);
        let warning = drawn
            .iter()
            .find(|(text, _)| text.starts_with("未更新到最新版本"))
            .expect("服务器上还有 5 项没拉下来，提交页要亮这句");
        assert!(warning.0.contains("5 项改动没拉到本地"), "{}", warning.0);

        // 已是最新的目录（dir 1）页面上不该有这句
        app.commit = Some(CommitPage::new(1, "测试".to_owned()));
        if let Some(page) = app.commit.as_mut() {
            page.set_entries(vec![entry(Item::Modified, true)]);
        }
        let drawn = commit_frame(&ctx, &mut app);
        assert!(
            !drawn
                .iter()
                .any(|(text, _)| text.starts_with("未更新到最新版本")),
            "已是最新还提示就成了误报"
        );
    }

    /// 计数说明要整句一行：这句以前画在右侧按钮列里，被 140px 挤成了两行。
    #[test]
    fn staging_note_is_drawn_on_one_line() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.commit = Some(page_of(
            vec![Item::Unversioned, Item::Missing],
            &[Item::Unversioned, Item::Missing],
        ));

        let drawn = commit_frame(&ctx, &mut app);
        let (text, rect) = drawn
            .iter()
            .find(|(text, _)| text.starts_with("提交时新增"))
            .map(|(text, rect)| (text.clone(), *rect))
            .expect("勾选了未版本化和已丢失各一项，提交页要显示这句计数");
        assert_eq!(text, "提交时新增 1 项 · 提交时删除 1 项", "换行会把这句拆开");
        assert!(rect.height() < 20.0, "这句只该占一行，实际高 {}", rect.height());
    }

    /// 标记与路径同一条基线：两处分成两只 label 时 egui 只能各自垂直居中，等宽 13 的标记
    /// 和比例字体 12.5 的路径字面高矮不同，路径就肉眼可见地掉下去了。
    #[test]
    fn mark_and_path_sit_on_one_baseline() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(vec![StatusEntry {
            path: "D:\\wc\\Alpha.java".to_owned(),
            name: "Alpha.java".to_owned(),
            item: Item::Modified,
            props: "none".to_owned(),
            checked: true,
        }]);
        app.commit = Some(page);
        app.apply_accent(&ctx);

        let mut out = ctx.run_ui(crate::testbed::base_input(), |ui| app.commit_page(ui));
        out.textures_delta.clear();
        // epaint 里 Glyph::pos.y 就是这一行的基线（相对行起点），行起点再加 galley 起点
        let mut mark: Option<f32> = None;
        let mut body: Option<f32> = None;
        for item in &out.shapes {
            if let egui::Shape::Text(text) = &item.shape {
                let line = text.galley.text();
                if line != "M" && !line.contains("Alpha.java") {
                    continue;
                }
                for row in &text.galley.rows {
                    for glyph in &row.row.glyphs {
                        let y = text.pos.y + row.pos.y + glyph.pos.y;
                        if glyph.chr == 'M' {
                            mark.get_or_insert(y);
                        }
                        if glyph.chr == 'D' {
                            body.get_or_insert(y);
                        }
                    }
                }
            }
        }
        let mark = mark.expect("列表里没有状态标记 M");
        let body = body.expect("列表里没有路径文字");
        assert!(
            (mark - body).abs() < 0.5,
            "标记基线 {mark:.2}、路径基线 {body:.2}，错开 {:.2} 像素",
            (mark - body).abs()
        );
    }

    /// 没更新到最新时点提交要被拦住：第一下只说后果，第二下才真的进提交流程。
    #[test]
    fn commit_button_gates_when_not_up_to_date() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.commit = Some(page_of(vec![Item::Modified], &[Item::Modified]));

        // 第一下：只确认，不起提交任务
        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "提交 1 项").center());
        assert!(app.commit.as_ref().unwrap().confirm_outdated, "第一下该只拦住");
        assert!(!app.pool.has(Kind::Commit, 0), "拦住的这一下不能起提交任务");
        assert!(app.log_tail().contains("再次点击"), "{}", app.log_tail());

        // 按钮文案与退路按钮要再跑一帧才画得出来：这一帧的文案是帧首算好的
        let drawn = commit_frame(&ctx, &mut app);
        assert!(has(&drawn, "仍要提交 1 项？"), "按钮文案要换成确认口径");
        assert!(has(&drawn, "先更新到最新") && has(&drawn, "放弃提交"), "要给出退路");

        // 第二下：放行。测试台的 app 没有 svn.exe，所以走到的是「无法提交」那句——
        // 正是它说明这一下已经越过拦截、进了 spawn_commit
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "仍要提交 1 项？").center());
        assert!(
            app.output
                .iter()
                .any(|line| line.text.contains("未更新到最新版本")),
            "放行时要往输出记录留痕：{:?}",
            app.output.iter().map(|line| line.text.clone()).collect::<Vec<_>>()
        );
        assert!(app.log_tail().contains("无法提交"), "第二下该越过拦截：{}", app.log_tail());
        assert!(!app.commit.as_ref().unwrap().confirm_outdated, "放行后确认态要清掉");
    }

    /// 拦住的那一下要能退出来：点「放弃提交」就松开，别第二次进来莫名其妙还是确认态。
    #[test]
    fn commit_gate_can_be_cancelled() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.commit = Some(page_of(vec![Item::Modified], &[Item::Modified]));

        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "提交 1 项").center());
        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "放弃提交").center());
        assert!(!app.commit.as_ref().unwrap().confirm_outdated, "放弃要真的松开");
        assert!(!app.pool.has(Kind::Commit, 0));

        // 松开之后按钮要回到普通文案，再点又能拦一次
        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "提交 1 项").center());
        assert!(app.commit.as_ref().unwrap().confirm_outdated);
        app.commit.as_mut().unwrap().set_entries(vec![entry(Item::Modified, true)]);
        assert!(
            !app.commit.as_ref().unwrap().confirm_outdated,
            "重新读过清单就不能留着上一次的确认"
        );
    }

    fn entry_at(item: Item, rel: &str) -> StatusEntry {
        StatusEntry {
            path: format!("D:\\wc\\{rel}"),
            name: rel.rsplit(['\\', '/']).next().unwrap_or(rel).to_owned(),
            item,
            props: "none".to_owned(),
            checked: true,
        }
    }

    /// 弹窗里一行一条的写法要变成 `Excludes` 认的语法：补 `-`、丢掉注释与空行与 `+`、
    /// 不带锚定的多段路径补成 `./`（不补就永不命中，issue #4），已经是 `-` / `.\` 开头的原样保留。
    #[test]
    fn normalize_excludes_turns_lines_into_the_bc_grammar() {
        assert_eq!(normalize_excludes("bin\\"), "-bin\\");
        assert_eq!(normalize_excludes("  *.log  "), "-*.log");
        assert_eq!(normalize_excludes("-bin\\"), "-bin\\", "已经带减号的不能变成 --");
        assert_eq!(normalize_excludes(r".\out\"), r"-.\out\");
        assert_eq!(normalize_excludes(r"A\B\target\"), r"-./A\B\target\");
        assert_eq!(normalize_excludes("a/b/c"), "-./a/b/c", "正斜杠的多段同样要锚定");
        // 注释、空行、不支持的包含模式都不该进规则串
        assert_eq!(normalize_excludes("# 构建产物\n\nbin\\\n+keep.txt\n"), "-bin\\");
        assert!(normalize_excludes("  \n#\n+\n").is_empty());
        // 老的分号写法照样认
        assert_eq!(normalize_excludes(r"-bin\;-.\auth.json"), r"-bin\;-.\auth.json");
    }

    /// 白名单那格与屏蔽那格共用同一套缝合规则，只是前缀反过来：`-` 行在这一格同样形同没写，
    /// 要挡东西得去上面那格——两格各管一边，才不会写出「以为挡住了其实没挡」的规则。
    #[test]
    fn normalize_includes_stitches_the_whitelist_box() {
        assert_eq!(normalize_includes("*.zip"), "+*.zip", "裸写法补 `+`");
        assert_eq!(normalize_includes("+*.zip"), "+*.zip", "已经带加号的不能变成 ++");
        assert_eq!(normalize_includes(r"A\B\target\"), r"+./A\B\target\");
        assert_eq!(normalize_includes("# 只合压缩包\n\n*.zip\n-bin\\\n"), "+*.zip");
        assert!(normalize_includes("  \n#\n-\n").is_empty());
    }

    /// 屏蔽只该挡住规则命中的那几条：单段目录名任意层级都算，根锚定的 `.\auth.json` 不能
    /// 带走子目录里的同名文件，裸多段路径补上锚定后深层同名目录也不该被误屏蔽。
    #[test]
    fn pending_entries_only_keeps_what_will_actually_be_committed() {
        let root = Path::new("D:\\wc");
        let raw = "bin\\\n# 日志\n*.log\n.\\auth.json\nInfrastructure\\Model\\target\\\n";
        let entries = vec![
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Unversioned, r"bin\tool.exe"),
            entry_at(Item::Unversioned, r"logs\debug.log"),
            entry_at(Item::Modified, r"auth.json"),
            entry_at(Item::Modified, r"src\auth.json"),
            entry_at(Item::Modified, r"Infrastructure\Model\target\out.jar"),
            entry_at(Item::Modified, r"src\Infrastructure\Model\target\deep.jar"),
            entry_at(Item::Normal, r"keep\untouched.java"),
            entry_at(Item::Conflict, r"keep\busted.java"),
        ];
        let kept = pending_entries(&entries, root, raw)
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>();
        assert_eq!(
            kept,
            vec![
                r"D:\wc\src\main.java".to_owned(),
                r"D:\wc\src\auth.json".to_owned(),
                r"D:\wc\src\Infrastructure\Model\target\deep.jar".to_owned(),
            ],
            "{kept:?}"
        );
        // 没写规则时口径就是老口径：只看能不能提交，一条都不多挡
        assert_eq!(
            pending_entries(&entries, root, "   ").len(),
            entries.iter().filter(|e| e.item.uploadable()).count()
        );
    }

    /// 屏蔽项就算带着 checked 也进不了提交清单：清单、add 名单、分类计数、可见行四个口径
    /// 共用同一份掩码，漏掉任何一个都是「看不见却传上去了」。
    #[test]
    fn excluded_entries_stay_out_of_the_commit_list() {
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(vec![
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Modified, r"bin\tool.exe"),
            entry_at(Item::Unversioned, r"logs\debug.log"),
        ]);
        page.apply_gate(Path::new(r"D:\wc"), "bin\\\n*.log\n");
        assert_eq!(page.hidden_count(), 2);
        assert_eq!(page.commit_targets(), vec![r"D:\wc\src\main.java".to_owned()]);
        assert!(page.selected_targets(Maintain::Add).is_empty(), "屏蔽的未版本化项不该补 svn add");
        let (added, modified, _, _) = page.counts();
        assert_eq!((added, modified), (0, 1), "分类计数只认没屏蔽的");
        assert_eq!(page.visible(), vec![0], "默认不显示屏蔽项");
        // 开关只是让人看见那几行，清单口径一个字都不能变
        page.show_excluded = true;
        assert_eq!(page.visible().len(), 3);
        assert_eq!(page.commit_targets(), vec![r"D:\wc\src\main.java".to_owned()]);
    }

    /// 「全选」按字面意义是把能勾的都勾上，但被本目录规则屏蔽的条目必须留在勾不上的那一侧。
    #[test]
    fn select_all_skips_excluded() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        app.cfg.dirs[0].commit_excludes = "bin\\".to_owned();
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(vec![
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Unversioned, r"bin\tool.exe"),
            entry_at(Item::Conflict, r"src\busted.java"),
        ]);
        page.entries.iter_mut().for_each(|entry| entry.checked = false);
        app.commit = Some(page);

        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "全选").center());
        let page = app.commit.as_ref().unwrap();
        assert!(page.entries[0].checked, "普通改动该被全选勾上");
        assert!(page.entries[2].checked, "冲突项按「全选」的字面意思也勾上");
        assert!(!page.entries[1].checked, "屏蔽项不能被全选勾上");
        assert_eq!(page.commit_targets(), vec![r"D:\wc\src\main.java".to_owned()]);

        // 「只选可提交项」同一道闸：屏蔽的未版本化项不回来
        let drawn = commit_frame(&ctx, &mut app);
        let _ = commit_click(&ctx, &mut app, rect_of(&drawn, "只选可提交项").center());
        let page = app.commit.as_ref().unwrap();
        assert!(page.entries[0].checked);
        assert!(!page.entries[1].checked, "屏蔽项连「恢复默认勾选」也不该被勾上");
    }

    /// 顶栏那枚开关：一条都没屏蔽时不占位；屏蔽了就把条数写在标签上。
    /// 默认被屏蔽的行连列表都不出现，点开才列出来并带上「已屏蔽」，此时它依然不在提交清单里。
    #[test]
    fn show_excluded_toggle_reveals_the_greyed_rows() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(vec![
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Modified, r"bin\tool.exe"),
        ]);
        app.commit = Some(page);

        // 还没写规则：既没有开关，两条改动都照常列着
        let drawn = commit_frame(&ctx, &mut app);
        assert!(!has(&drawn, "显示屏蔽的 1 项"), "没屏蔽任何东西时不该占一个开关位");
        assert!(has(&drawn, "本次修改内容：共 2 项"));

        app.cfg.dirs[0].commit_excludes = "bin\\".to_owned();
        let drawn = commit_frame(&ctx, &mut app);
        assert!(has(&drawn, "显示屏蔽的 1 项"), "{:?}", texts_of(&drawn));
        assert!(has(&drawn, "本次修改内容：共 1 项"), "总数要扣掉被屏蔽的那条");
        assert!(
            !drawn.iter().any(|(text, _)| text.contains("tool.exe")),
            "被屏蔽的行默认不该出现在列表里：{:?}",
            texts_of(&drawn)
        );

        let drawn = commit_click(&ctx, &mut app, rect_of(&drawn, "显示屏蔽的 1 项").center());
        assert!(
            drawn.iter().any(|(text, _)| text.contains("tool.exe")),
            "开关打开后那一行要能看见：{:?}",
            texts_of(&drawn)
        );
        assert!(has(&drawn, "已屏蔽"));
        // 让人看见不等于让人勾上：清单口径一个字都不能变
        let page = app.commit.as_ref().unwrap();
        assert_eq!(page.commit_targets(), vec![r"D:\wc\src\main.java".to_owned()]);
    }

    /// 过滤框是单行的，多个关键字用 `;` 隔开、任一命中即留下（OR）；空段丢掉、统一转小写，
    /// 中文关键字照样能命中。
    #[test]
    fn multi_keyword_filter_keeps_any_needle_that_matches() {
        assert_eq!(
            filter_needles("  ;; 报告 ;  Main  ; "),
            vec!["报告".to_owned(), "main".to_owned()],
            "空段不该占一条，大小写折叠后再比对"
        );
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(vec![
            entry_at(Item::Modified, r"src\视图\报告.java"),
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Modified, r"docs\note.md"),
        ]);
        page.filter = "报告;note".to_owned();
        assert_eq!(page.visible(), vec![0, 2], "两个关键字是或关系");
        page.filter = "MAIN".to_owned();
        assert_eq!(page.visible(), vec![1], "过滤不区分大小写");
        page.filter = "不存在的关键字".to_owned();
        assert!(page.visible().is_empty());
    }

    /// 弹窗里敲的一行屏蔽规则要走完整条链：写回目录配置 → 落盘 → 下一帧提交页按它重算掩码。
    /// 中间断掉任何一环都是「写了规则却照提」。
    #[test]
    fn commit_rules_dialog_persists_excludes() {
        let _appdata = crate::testbed::lock_appdata();
        // 落盘这件事不能被上次运行留下的 config.json 顶包：先清干净再指过去
        let appdata = std::env::temp_dir().join("repo_manager_commit_rules_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        let entries = vec![
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Unversioned, r"bin\tool.exe"),
        ];
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(entries.clone());
        app.commit = Some(page);
        // 窗口里的命中数按最近一次读取的完整清单现算，这就是检测 / 现读落进 DirView 的那份
        app.dirs[0].changed = Some(2);
        app.dirs[0].all_changes = entries;
        // 入口在目录行的「更多」里，菜单链路另有用例；这里直接从同一份状态起窗口
        app.commit_rules = Some(crate::CommitRules::new(0, ""));
        // 新窗口第一帧只是把尺寸算出来，形状要第二帧才落纸
        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        assert!(has(&drawn, "差异比对过滤条件 · 存本目录配置"), "{:?}", texts_of(&drawn));
        assert!(has(&drawn, "当前未命中任何条目"));

        // 点进屏蔽框拿焦点，再喂一个文本事件
        let field = rect_containing(&drawn, "*.log").center();
        let mut click = Vec::new();
        pointer(&mut click, field);
        let _ = rules_frame(&ctx, &mut app, click);
        let _ = rules_frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("bin\\".to_owned())],
        );

        assert_eq!(app.cfg.dirs[0].commit_excludes, "bin\\", "窗口里那行要写回目录配置");
        let saved = std::fs::read_to_string(crate::config::config_file()).unwrap_or_default();
        assert!(
            saved.contains(r#""commit_excludes": "bin\\""#),
            "改了配置就得真的落盘：{saved}"
        );

        // 下一帧：提交页按新规则重算掩码，窗口预览与提交清单跟着同一个口径走
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        assert!(
            has(&drawn, "当前命中 1 项：不进入提交清单，提交页中亦不显示"),
            "{:?}",
            texts_of(&drawn)
        );
        let page = app.commit.as_ref().unwrap();
        assert_eq!(page.hidden_count(), 1);
        assert_eq!(page.commit_targets(), vec![r"D:\wc\src\main.java".to_owned()]);
    }

    /// 弹窗两格各写各的字段：上面那格是目录行同款的 BC 筛选条件（配置里是 `;` 串、
    /// 框里一行一条），下面那格才是屏蔽规则。串了格就是「调了差异口径却把文件屏蔽掉了」。
    #[test]
    fn commit_rules_dialog_keeps_the_two_fields_apart() {
        let _appdata = crate::testbed::lock_appdata();
        let appdata = std::env::temp_dir().join("repo_manager_bc_filter_dialog_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        // 框里两行 = 打开窗口时从一份两行的 BC 串拆出来的行视图
        app.commit_rules = Some(crate::CommitRules::new(0, "-*.iml\n-bin\\"));

        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        assert!(has(&drawn, "屏蔽可提交项 · 存本目录配置"), "{:?}", texts_of(&drawn));

        assert_eq!(
            app.cfg.dirs[0].bc_filter, "-*.iml;-bin\\",
            "行视图要拼回 BC 那套分号串，换行留在配置里会把规则拆散"
        );
        assert!(
            app.cfg.dirs[0].commit_excludes.is_empty(),
            "过滤条件那格不该顺手写屏蔽规则"
        );
        let saved = std::fs::read_to_string(crate::config::config_file()).unwrap_or_default();
        assert!(
            saved.contains(r#""bc_filter": "-*.iml;-bin\\""#),
            "改了筛选条件就得真的落盘：{saved}"
        );

        // 反过来：屏蔽框一个字都不该漏进 bc_filter
        app.cfg.dirs[0].bc_filter.clear();
        app.commit_rules = Some(crate::CommitRules::new(0, ""));
        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let field = rect_containing(&rules_frame(&ctx, &mut app, Vec::new()), "*.log").center();
        let mut click = Vec::new();
        pointer(&mut click, field);
        let _ = rules_frame(&ctx, &mut app, click);
        let _ = rules_frame(&ctx, &mut app, vec![egui::Event::Text("logs\\".to_owned())]);
        assert_eq!(app.cfg.dirs[0].commit_excludes, "logs\\");
        assert!(app.cfg.dirs[0].bc_filter.is_empty(), "屏蔽规则不该写进 BC 筛选条件");
    }

    /// 第二页那两格是双向合并的白名单与黑名单：各写各的字段，既不许串到第一页两格，也不许互相串。
    /// 更要紧的是这两格一个字都不该动提交口径——串了就是「以为只划了合并范围，其实把文件挡掉了提交」
    #[test]
    fn merge_scope_boxes_write_only_their_own_fields() {
        let _appdata = crate::testbed::lock_appdata();
        let appdata = std::env::temp_dir().join("repo_manager_merge_scope_dialog_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        let entries = vec![
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Unversioned, r"bin\tool.exe"),
        ];
        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(entries.clone());
        app.commit = Some(page);
        app.dirs[0].changed = Some(2);
        app.dirs[0].all_changes = entries;
        app.commit_rules = Some(crate::CommitRules::new(0, "-*.iml"));

        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        // 两格在第二页：不点标签过去，画面上根本不该有它们
        assert!(!has(&drawn, "参与合并的文件 · 存本目录配置"), "{:?}", texts_of(&drawn));
        let mut tap = Vec::new();
        pointer(&mut tap, rect_of(&drawn, "双向合并范围").center());
        let _ = rules_frame(&ctx, &mut app, tap);
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        assert!(has(&drawn, "参与合并的文件 · 存本目录配置"), "{:?}", texts_of(&drawn));
        assert!(has(&drawn, "不参与合并的文件 · 存本目录配置"), "{:?}", texts_of(&drawn));

        // 白名单格：敲一行进去
        let field = rect_containing(&drawn, "*.java").center();
        let mut click = Vec::new();
        pointer(&mut click, field);
        let _ = rules_frame(&ctx, &mut app, click);
        let _ = rules_frame(&ctx, &mut app, vec![egui::Event::Text("*.zip".to_owned())]);
        assert_eq!(app.cfg.dirs[0].merge_include, "*.zip", "白名单那一格写的是 merge_include");
        assert!(app.cfg.dirs[0].merge_exclude.is_empty(), "白名单不该顺手写黑名单");
        assert_eq!(app.cfg.dirs[0].bc_filter, "-*.iml", "合并范围不该动差异口径");
        assert!(app.cfg.dirs[0].commit_excludes.is_empty(), "合并范围不该动提交屏蔽");

        // 黑名单格：敲一行进去
        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        let field = rect_containing(&drawn, "*.min.js").center();
        let mut click = Vec::new();
        pointer(&mut click, field);
        let _ = rules_frame(&ctx, &mut app, click);
        let _ = rules_frame(&ctx, &mut app, vec![egui::Event::Text(r"target\".to_owned())]);
        assert_eq!(app.cfg.dirs[0].merge_exclude, r"target\", "黑名单那一格写的是 merge_exclude");
        assert_eq!(app.cfg.dirs[0].merge_include, "*.zip", "黑名单不该反过来改白名单");

        let saved = std::fs::read_to_string(crate::config::config_file()).unwrap_or_default();
        assert!(
            saved.contains(r#""merge_include": "*.zip""#)
                && saved.contains(r#""merge_exclude": "target\\""#),
            "两格都写满了就得真的落盘：{saved}"
        );

        // 提交页一个字节都不该跟着变：两格只划合并范围，屏蔽计数与提交清单用的还是提交那一格
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        let page = app.commit.as_ref().unwrap();
        assert_eq!(page.hidden_count(), 0, "{:?}", texts_of(&drawn));
        assert_eq!(
            page.commit_targets(),
            vec![
                r"D:\wc\src\main.java".to_owned(),
                r"D:\wc\bin\tool.exe".to_owned()
            ]
        );
    }

    /// 四格竖着一列排时窗口要 658px 高，分作两页后每页实测 401px。这里锁三件事：每页只画得出
    /// 属于它的那两格、切页整块换掉而窗口不跳高、切回去时先前敲的内容一个字都不丢。
    #[test]
    fn the_rules_window_splits_into_two_pages_of_the_same_height() {
        let _appdata = crate::testbed::lock_appdata();
        let appdata = std::env::temp_dir().join("repo_manager_rules_pages_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        app.cfg.dirs[0].commit_excludes = "logs\\".to_owned();
        app.cfg.dirs[0].merge_include = "*.zip".to_owned();
        app.commit_rules = Some(crate::CommitRules::new(0, "-*.iml"));

        // 「这一页有多高」量的是窗口自己的内容：标题行顶到「确认」按钮底
        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let span = |drawn: &[(String, egui::Rect)]| {
            rect_of(drawn, "确认").max.y
                - rect_containing(drawn, "差异过滤、提交屏蔽与合并范围").min.y
        };
        let page_names = |drawn: &[(String, egui::Rect)]| -> Vec<String> {
            drawn
                .iter()
                .filter(|(text, _)| text.ends_with("· 存本目录配置"))
                .map(|(text, _)| text.clone())
                .collect()
        };
        let tap = |app: &mut RepoApp, name: &str| {
            let drawn = rules_frame(&ctx, app, Vec::new());
            let mut click = Vec::new();
            pointer(&mut click, rect_of(&drawn, name).center());
            let _ = rules_frame(&ctx, app, click);
            rules_frame(&ctx, app, Vec::new())
        };

        let first = rules_frame(&ctx, &mut app, Vec::new());
        assert_eq!(
            page_names(&first),
            vec![
                "差异比对过滤条件 · 存本目录配置".to_owned(),
                "屏蔽可提交项 · 存本目录配置".to_owned()
            ],
            "{:?}",
            texts_of(&first)
        );
        let height = span(&first);
        assert!(height < 430.0, "一页只该有两格，实际高 {height:.0}px");

        let second = tap(&mut app, "双向合并范围");
        assert_eq!(
            page_names(&second),
            vec![
                "参与合并的文件 · 存本目录配置".to_owned(),
                "不参与合并的文件 · 存本目录配置".to_owned()
            ],
            "{:?}",
            texts_of(&second)
        );
        assert!(
            (span(&second) - height).abs() < 12.0,
            "切页跳高就等于窗口在屏幕上抖：{} → {}",
            height,
            span(&second)
        );
        // 屏蔽规则在第一页、白名单在第二页，值都存在本目录配置里，切来切去不该丢
        let back = tap(&mut app, "差异过滤与提交屏蔽");
        assert!(has(&back, "屏蔽可提交项 · 存本目录配置"));
        assert_eq!(app.cfg.dirs[0].commit_excludes, "logs\\");
        assert_eq!(app.cfg.dirs[0].merge_include, "*.zip", "切页不许动另一页的规则");
        assert_eq!(
            app.commit_rules.as_ref().unwrap().filter_buf,
            "-*.iml",
            "过滤条件的行视图存在窗口状态里，切页回来还得是原样"
        );
    }

    /// 标题栏那枚 ✕ 要真能把窗口关掉。叉不是文字，测试台取不到它，只能从这一帧画出的线段里
    /// 把它认出来——顺带正好量到它的可点范围：出厂的 Heading 标题比叉高出一大截，叉却只有
    /// `icon_width` 那一小点，指针落在标题带上就只是拖动窗口，看上去就是「点了没反应」。
    /// 标题收到 14 之后，叉得基本占住标题那一行，这一条就是防它再被大标题架空。
    #[test]
    fn the_title_bar_cross_closes_the_rules_window() {
        let _appdata = crate::testbed::lock_appdata();
        let appdata = std::env::temp_dir().join("repo_manager_rules_cross_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        // 观感由 tune_style 定，而它在 RepoApp::new 里才调；测试自造的 ctx 得自己补一次，
        // 否则量到的是出厂字号的标题栏，跟屏幕上看到的不是一个窗口
        app.apply_accent(&ctx);
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        app.commit_rules = Some(crate::CommitRules::new(0, "-*.iml"));

        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        let title = rect_containing(&drawn, "差异过滤、提交屏蔽与合并范围");
        let mut out = ctx.run_ui(crate::testbed::base_input(), |ui| {
            app.commit_page(ui);
            app.commit_rules_dialog(ui.ctx());
        });
        out.textures_delta.clear();
        let window = ctx
            .memory(|m| m.area_rect(egui::Id::new("commit_rules")))
            .expect("窗口应已登记在 area 里");
        // 标题带里画出来的线段只有叉的那两笔
        let cross = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::LineSegment { .. } => {
                    let rect = item.shape.visual_bounding_rect();
                    (rect.center().y < window.min.y + 30.0).then_some(rect)
                }
                _ => None,
            })
            .reduce(|a, b| a.union(b))
            .expect("标题栏右上角应画出 ✕");

        assert!(
            cross.min.x > title.max.x && cross.max.y <= window.min.y + 40.0,
            "叉要在标题带的最右端：叉 {cross:?}、标题 {title:?}、窗口顶 {}",
            window.min.y
        );
        assert!(
            cross.height() >= title.height() - 4.0,
            "叉只有 {} 高，标题带却有 {} 高——点在标题带上只是拖窗口，等于没有出口",
            cross.height(),
            title.height()
        );

        let mut click = Vec::new();
        pointer(&mut click, cross.center());
        let _ = rules_frame(&ctx, &mut app, click);
        assert!(app.commit_rules.is_none(), "点叉要把窗口收掉");
        assert!(
            !app.pool.has(crate::jobs::Kind::Refresh, 0),
            "叉不是「确认」，关掉就好，不该起按新规则的重读"
        );
    }

    /// 格子里写多写少不许动到整窗几何：同一个框摆两行和摆四十行，页面总高、「确认」离窗口
    /// 右下角的那点距离都得一样——尺寸钉死在框上，写满只在框里滚。
    #[test]
    fn a_packed_rule_field_keeps_the_window_geometry() {
        let _appdata = crate::testbed::lock_appdata();
        let appdata = std::env::temp_dir().join("repo_manager_rules_packed_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.apply_accent(&ctx);
        // 测试台量到的是「确认」二字的文字矩形，按钮框要比它外扩一圈 button_padding
        let pad = ctx.style_of(egui::Theme::Dark).spacing.button_padding;
        app.cfg.dirs[0].path = r"D:\wc".to_owned();
        app.commit_rules = Some(crate::CommitRules::new(0, "-*.iml\n-*.log"));

        // 都以窗口自己的矩形为参照：窗口头两帧还在校准居中位置，位置会挪而尺寸不会
        let measure = |app: &mut RepoApp| {
            let _ = rules_frame(&ctx, app, Vec::new());
            let drawn = rules_frame(&ctx, app, Vec::new());
            let window = ctx
                .memory(|m| m.area_rect(egui::Id::new("commit_rules")))
                .unwrap();
            let confirm = rect_of(&drawn, "确认");
            let page = (
                confirm.max.y - rect_containing(&drawn, "差异过滤、提交屏蔽与合并范围").min.y,
                window.max.y - confirm.max.y,
                window.max.x - confirm.max.x,
            );
            (page, texts_of(&drawn))
        };

        let (sparse, texts) = measure(&mut app);
        app.commit_rules.as_mut().unwrap().filter_buf = (0..40)
            .map(|i| format!("-src/module{i}/some/long/package/ServiceImpl.java"))
            .collect::<Vec<_>>()
            .join("\n");
        let (packed, _) = measure(&mut app);

        for (name, a, b) in [
            ("页高", sparse.0, packed.0),
            ("离底", sparse.1, packed.1),
            ("离右", sparse.2, packed.2),
        ] {
            assert!((a - b).abs() < 0.5, "灌满四十行后{name}变了：{a:.1} → {b:.1}｜{texts:?}");
        }
        // 测试台量到的是「确认」二字的文字矩形，按钮框还要再往外扩一圈 button_padding
        let (right, bottom) = (sparse.2 - pad.x, sparse.1 - pad.y);
        assert!(
            right <= 8.0 && bottom <= 8.0,
            "「确认」得贴在窗口右下角：按钮框离右 {right:.1}、离底 {bottom:.1}"
        );
    }

    /// 「确认」是这扇窗的出口：按下要关窗，并按刚写下的规则重读本目录（起 Refresh 任务，
    /// 它内部连本地差异比对一起跑）。规则改了却不重读，主页「待提交 N」和窗口里的命中数
    /// 就还停在旧口径上，看上去像规则没生效
    #[test]
    fn commit_rules_dialog_confirm_closes_and_refreshes() {
        let _appdata = crate::testbed::lock_appdata();
        let appdata = std::env::temp_dir().join("repo_manager_rules_confirm_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        // 检测要求目录真实存在；svn.exe 只被 `available()` 查一句「文件在不在」，
        // 指一个非 exe 的桩文件——进程起不来会立刻带错误返回，测试不等它落地
        let wc = appdata.join("wc");
        std::fs::create_dir_all(&wc).unwrap();
        let stub = appdata.join("svn-stub.txt");
        std::fs::write(&stub, "stub").unwrap();

        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = crate::testbed::stub_app(false);
        app.cfg.dirs[0].path = wc.to_string_lossy().into_owned();
        app.svn.exe = stub;
        app.commit_rules = Some(crate::CommitRules::new(0, ""));
        // 新窗口第一帧只是把尺寸算出来，形状要第二帧才落纸
        let _ = rules_frame(&ctx, &mut app, Vec::new());
        let drawn = rules_frame(&ctx, &mut app, Vec::new());
        let confirm = drawn
            .iter()
            .find(|(text, _)| text == "确认")
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("窗口底部应有「确认」，实际：{:?}", texts_of(&drawn)));
        assert!(
            !app.pool.has(crate::jobs::Kind::Refresh, 0),
            "只是开着窗口，不该起检测"
        );

        let mut click = Vec::new();
        pointer(&mut click, confirm.center());
        let _ = rules_frame(&ctx, &mut app, click);

        assert!(app.commit_rules.is_none(), "点确认要顺手把窗口收掉");
        assert!(
            app.pool.has(crate::jobs::Kind::Refresh, 0),
            "确认要按新规则重读一次本目录"
        );
    }

    /// 同一批条目、同一个根、同一条规则，三个出口必须给出同一份清单：主页的「待提交 N」、
    /// 「全部上传」的三份名单、提交页勾选出来的清单。旁路少滤一次就是「屏幕上看不见却照样传上去」。
    #[test]
    fn home_count_and_upload_all_agree_with_the_page() {
        let root = Path::new("D:\\wc");
        let raw = "bin\\\n# 编译日志\n*.log\nInfrastructure\\Model\\target\\\n";
        let entries = vec![
            entry_at(Item::Modified, r"src\main.java"),
            entry_at(Item::Unversioned, r"bin\tool.exe"),
            entry_at(Item::Missing, r"logs\debug.log"),
            entry_at(Item::Modified, r"Infrastructure\Model\target\out.jar"),
            entry_at(Item::Unversioned, r"src\obj\Debug\a.class"),
            entry_at(Item::Missing, r"src\gone.java"),
            entry_at(Item::Conflict, r"src\busted.java"),
            entry_at(Item::Normal, r"src\untouched.java"),
        ];

        let mut page = CommitPage::new(0, "测试".to_owned());
        page.set_entries(entries.clone());
        page.apply_gate(root, raw);
        let plan = upload_plan(&entries, root, raw);
        let home = pending_entries(&entries, root, raw)
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>();

        assert_eq!(page.hidden_count(), 3, "bin、*.log、根锚定的多段目录各挡一条");
        let kept = vec![
            r"D:\wc\src\main.java".to_owned(),
            r"D:\wc\src\obj\Debug\a.class".to_owned(),
            r"D:\wc\src\gone.java".to_owned(),
        ];
        assert_eq!(home, kept, "主页的数得和这份清单一样：{home:?}");
        assert_eq!(plan.targets, kept, "「全部上传」要提交的就是这几条");
        assert_eq!(page.commit_targets(), kept, "提交页勾选出来的必须是同一批");
        assert_eq!(plan.adds, vec![r"D:\wc\src\obj\Debug\a.class".to_owned()]);
        assert_eq!(plan.deletes, vec![r"D:\wc\src\gone.java".to_owned()]);
        assert_eq!(page.selected_targets(Maintain::Add), plan.adds);
        assert_eq!(page.selected_targets(Maintain::Delete), plan.deletes);
        // 冲突项本来就提交不了，规则也别说它；两边都别把它算进任何一份名单
        for list in [&home, &plan.targets, &page.commit_targets()] {
            assert!(list.iter().all(|path| !path.contains("busted")), "{list:?}");
            assert!(list.iter().all(|path| !path.contains("untouched")), "{list:?}");
        }
    }
}
