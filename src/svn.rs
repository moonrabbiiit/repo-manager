use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use encoding_rs::GBK;

use crate::merge::{Ancestor, WcState};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// svn 输出在中文 Windows 上可能是 GBK，`--xml` 恒为 UTF-8，两种都要能解。
pub fn decode_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => GBK.decode(bytes).0.into_owned(),
    }
}

pub(crate) fn temp_file(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{}_{}_{}", prefix, std::process::id(), nanos))
}

// ------------------------------------------------------------------------ 运行结果

pub struct Run {
    pub ok: bool,
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl Run {
    fn new(ok: bool, code: i32, out: String, err: String) -> Self {
        Self { ok, code, out, err }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(false, -1, String::new(), message.into())
    }

    /// 供界面展示的错误摘要。
    pub fn summary(&self) -> String {
        let err = self.err.trim();
        if !err.is_empty() {
            return err.to_owned();
        }
        let out = self.out.trim();
        if !out.is_empty() {
            return out.to_owned();
        }
        format!("命令退出码 {}", self.code)
    }
}

// ------------------------------------------------------------------------ 数据结构

#[derive(Clone, Debug, Default)]
pub struct WcInfo {
    pub is_wc: bool,
    pub url: String,
    pub repos_root: String,
    pub revision: String,
    /// Git 目录的当前分支（`rev-parse --abbrev-ref HEAD`，分离头指针时是 "HEAD"）；
    /// SVN 没有分支概念，恒为空
    pub branch: String,
    pub node_kind: String,
    pub last_rev: String,
    pub last_author: String,
    pub last_date: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Added,
    Modified,
    Deleted,
    Replaced,
    Unversioned,
    Missing,
    Conflict,
    Ignored,
    External,
    Incomplete,
    Normal,
    Other,
}

impl Item {
    pub fn from_svn(raw: &str) -> Self {
        match raw {
            "added" => Self::Added,
            "modified" => Self::Modified,
            "deleted" => Self::Deleted,
            "replaced" => Self::Replaced,
            "unversioned" => Self::Unversioned,
            "missing" => Self::Missing,
            "conflicted" => Self::Conflict,
            "ignored" => Self::Ignored,
            "external" => Self::External,
            "incomplete" => Self::Incomplete,
            "normal" => Self::Normal,
            _ => Self::Other,
        }
    }

    pub fn mark(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Replaced => 'R',
            Self::Unversioned => '?',
            Self::Missing => '!',
            Self::Conflict => 'C',
            Self::Ignored => 'I',
            Self::External => 'X',
            Self::Incomplete => '~',
            Self::Normal | Self::Other => ' ',
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Self::Added => "新增",
            Self::Modified => "修改",
            Self::Deleted => "删除",
            Self::Replaced => "替换",
            Self::Unversioned => "未版本化",
            Self::Missing => "已丢失",
            Self::Conflict => "冲突",
            Self::Ignored => "已忽略",
            Self::External => "外部引用",
            Self::Incomplete => "不完整",
            Self::Normal => "正常",
            Self::Other => "未知",
        }
    }

    /// 可直接提交的状态。
    pub fn committable(self) -> bool {
        matches!(self, Self::Added | Self::Modified | Self::Deleted | Self::Replaced)
    }

    /// 需要先 `svn add` 才能提交。
    pub fn needs_add(self) -> bool {
        matches!(self, Self::Unversioned)
    }

    /// 需要 `svn delete` 记录删除。
    pub fn needs_delete(self) -> bool {
        matches!(self, Self::Missing)
    }

    /// 「全部上传」的口径：可直接提交，或提交前补一次 add / delete 就能一起提交。
    /// 冲突、不完整、已忽略、外部引用都不算——它们必须先人工处理，不能自动带上服务器。
    pub fn uploadable(self) -> bool {
        self.committable() || self.needs_add() || self.needs_delete()
    }

    pub fn color(self) -> (u8, u8, u8) {
        match self {
            Self::Added => (60, 190, 110),
            Self::Modified => (90, 160, 240),
            Self::Deleted => (235, 90, 90),
            Self::Replaced => (185, 140, 240),
            Self::Conflict => (255, 80, 160),
            Self::Missing => (235, 90, 90),
            Self::Unversioned => (150, 150, 150),
            _ => (170, 170, 170),
        }
    }

    /// 必须人工干预才动得了的条目：冲突（含 tree conflict，`svn status --xml` 里同样是
    /// `conflicted`）与不完整。目录行、顶栏汇总和提交页「需处理」都按这个口径数。
    pub fn blocked(self) -> bool {
        matches!(self, Self::Conflict | Self::Incomplete)
    }
}

/// 需要人工处理的条目数。
pub fn blocked_count(entries: &[StatusEntry]) -> usize {
    entries.iter().filter(|entry| entry.item.blocked()).count()
}

/// 需要人工处理的条目本身（口径同 `blocked_count`）：「解决冲突」窗口列的就是这一批。
pub fn blocked_entries(entries: &[StatusEntry]) -> Vec<StatusEntry> {
    entries
        .iter()
        .filter(|entry| entry.item.blocked())
        .cloned()
        .collect()
}

/// 一条冲突的收场策略。前三种是 `svn resolve --accept <取值>` 的不同取值，第四种另走
/// `svn revert`——想把本地这份整个扔掉、回到干净基线，SVN 只有这一个开关。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Accept {
    /// 保留手头这份，只把冲突状态摘掉（冲突标记会留在文件里，得自己清）
    #[default]
    Working,
    /// 整份用我自己的版本，服务器带来的改动丢掉
    MineFull,
    /// 整份用服务器版本，我在这份文件里的改动丢掉
    TheirsFull,
    /// 丢弃本地改动，文件回到干净基线（标记和改动一起没了）
    Revert,
}

impl Accept {
    pub const ALL: [Accept; 4] = [Self::Working, Self::MineFull, Self::TheirsFull, Self::Revert];

    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "保留手头这份",
            Self::MineFull => "用我的版本",
            Self::TheirsFull => "用服务器版本",
            Self::Revert => "丢弃本地改动",
        }
    }

    /// 后果说明：这几种策略都会改动本地文件，两种「用谁的版本」还会丢掉一边，得写明白
    pub fn tip(self) -> &'static str {
        match self {
            Self::Working => "把现在磁盘上这份当已解决，冲突标记 <<<<<<< 留在文件里，\n之后自己删或用 BC 合。",
            Self::MineFull => "整份用合并前我自己的版本，服务器那次改动从这份文件里消失。",
            Self::TheirsFull => "整份用服务器版本，我在这份文件里的改动消失。",
            Self::Revert => "svn revert：本地改动全部丢弃，文件回到工作副本的干净基线。",
        }
    }

    /// 发给 `--accept` 的取值；Revert 用的是另一条命令，没有这个参数
    fn flag(self) -> Option<&'static str> {
        match self {
            Self::Working => Some("working"),
            Self::MineFull => Some("mine-full"),
            Self::TheirsFull => Some("theirs-full"),
            Self::Revert => None,
        }
    }

    /// 输出区里那行命令的意思（`resolve --accept working` / `revert`）
    pub fn command(self) -> String {
        match self.flag() {
            Some(flag) => format!("resolve --accept {flag}"),
            None => "revert".to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StatusEntry {
    pub path: String,
    pub name: String,
    pub item: Item,
    pub props: String,
    pub checked: bool,
}

#[derive(Clone, Debug)]
pub struct LogPath {
    pub action: char,
    pub kind: String,
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub revision: String,
    pub author: String,
    pub date: String,
    pub message: String,
    pub paths: Vec<LogPath>,
}

// ------------------------------------------------------------------------ XML 解析

fn first_text(doc: &roxmltree::Document<'_>, tag: &str) -> String {
    doc.descendants()
        .find(|node| node.has_tag_name(tag))
        .and_then(|node| node.text())
        .unwrap_or("")
        .to_owned()
}

fn first_child_text(node: &roxmltree::Node<'_, '_>, tag: &str) -> String {
    node.children()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .unwrap_or("")
        .to_owned()
}

pub fn parse_info(xml: &str) -> Option<WcInfo> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let entry = doc.descendants().find(|node| node.has_tag_name("entry"))?;
    let commit = doc.descendants().find(|node| node.has_tag_name("commit"));
    Some(WcInfo {
        is_wc: true,
        url: first_text(&doc, "url"),
        repos_root: first_text(&doc, "root"),
        revision: entry.attribute("revision").unwrap_or("").to_owned(),
        branch: String::new(),
        node_kind: entry.attribute("kind").unwrap_or("").to_owned(),
        last_rev: commit
            .as_ref()
            .and_then(|node| node.attribute("revision"))
            .unwrap_or("")
            .to_owned(),
        last_author: first_text(&doc, "author"),
        last_date: format_svn_date(&first_text(&doc, "date")),
    })
}

/// `svn status -u --xml`：带 <repos-status> 子节点的 entry，就是服务器上已经变了、
/// 本地还没更新的条目。这个数只能由 `-u` 给：光比 `svn info` 的工作副本 Revision 和仓库
/// HEAD，在「不相等」的方向会误报——提交只推进被提交路径的版本号，工作副本根目录还停在
/// 旧版本上，HEAD 一动就显示「可更新」。反方向（相等 ⇒ 没有可更新项）是成立的，见 [`wc_at_head`]。
pub fn parse_out_of_date(xml: &str) -> usize {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return 0;
    };
    doc.descendants()
        .filter(|node| node.has_tag_name("entry"))
        .filter(|node| {
            node.children()
                .any(|child| child.has_tag_name("repos-status"))
        })
        .count()
}

/// 本地基线是不是就是仓库 HEAD。`svn info --xml` 的 `entry/@revision` 在工作副本上给出的是
/// 本地 BASE、在仓库 URL 上给出的是 HEAD，相等时服务器上不可能还有比本地更新的版本，检测
/// 就能只读本地状态，省掉 `svn status -u` 那半趟整树比较（七万文件的工作副本上 0.6~3.4 秒）。
/// 只用于「相等就跳过 -u」这一个方向：不相等说明不了有东西可更新（见 [`parse_out_of_date`]），
/// 任一侧没读到版本号也返回 false——「不知道」不能当成「已是最新」。
/// 例外是混合版本的工作副本：子目录被单独 peg update / switch 到比根目录更旧的版本时，
/// 根目录照样等于 HEAD，只有 `-u` 看得见它。
pub fn wc_at_head(local_rev: &str, head_rev: &str) -> bool {
    !local_rev.is_empty() && !head_rev.is_empty() && local_rev == head_rev
}

/// `svn status --xml`：entry 的 path 属性可能是绝对路径，也可能是相对路径。
pub fn parse_status(xml: &str, base: &Path) -> Vec<StatusEntry> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("entry")) {
        let Some(raw) = node.attribute("path") else {
            continue;
        };
        let status = node.children().find(|n| n.has_tag_name("wc-status"));
        let item = Item::from_svn(
            status
                .as_ref()
                .and_then(|s| s.attribute("item"))
                .unwrap_or("none"),
        );
        let item = if status.as_ref().and_then(|s| s.attribute("tree-conflicted")) == Some("true") {
            Item::Conflict
        } else { item };
        let props = status
            .as_ref()
            .and_then(|s| s.attribute("props"))
            .unwrap_or("none")
            .to_owned();
        let path = if raw.is_empty() || raw == "." {
            base.display().to_string()
        } else if Path::new(raw).is_absolute() {
            raw.to_owned()
        } else {
            base.join(raw).display().to_string()
        };
        let name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        entries.push(StatusEntry {
            path,
            name,
            item,
            props,
            // 默认勾上所有「能随上传走」的条目：A/M/D/R 直接提交，?(自动 add)、!(自动 delete)
            // 也一起带上；冲突、不完整、忽略、外部引用必须先人工处理，仍不勾（勾了也提交不了）。
            checked: item.uploadable(),
        });
    }
    entries.sort_by(|a, b| a.item.text().cmp(b.item.text()).then(a.path.cmp(&b.path)));
    entries
}

pub fn parse_log(xml: &str) -> Vec<LogEntry> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut logs = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("logentry")) {
        let mut paths = Vec::new();
        for path in node.descendants().filter(|n| n.has_tag_name("path")) {
            let action = path
                .attribute("action")
                .unwrap_or("")
                .chars()
                .next()
                .unwrap_or(' ');
            paths.push(LogPath {
                action,
                kind: path.attribute("kind").unwrap_or("").to_owned(),
                path: path.text().unwrap_or("").to_owned(),
            });
        }
        logs.push(LogEntry {
            revision: node.attribute("revision").unwrap_or("").to_owned(),
            author: first_child_text(&node, "author"),
            date: format_svn_date(&first_child_text(&node, "date")),
            message: first_child_text(&node, "msg").trim().to_owned(),
            paths,
        });
    }
    logs
}

/// svn 返回的 UTC 时间可能带 7 位小数秒，统一转为本地时间字符串。
pub fn format_svn_date(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&trim_fractional(raw)) {
        return parsed
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
    }
    raw.to_owned()
}

fn trim_fractional(raw: &str) -> String {
    let Some(dot) = raw.find('.') else {
        return raw.to_owned();
    };
    let tail = raw[dot + 1..]
        .find(|c| c == 'Z' || c == '+' || c == '-')
        .map(|index| dot + 1 + index)
        .unwrap_or(raw.len());
    if tail - (dot + 1) <= 6 {
        return raw.to_owned();
    }
    format!("{}{}", &raw[..dot + 6], &raw[tail..])
}

/// 把 `svn log` 里的路径（相对**仓库根**，如 `/code/HRP.FA/...`）换算成本地工作副本里的路径。
/// `repos_root` / `wc_url` 取自该目录的 `svn info`，`local` 是它的本地路径。
/// 反过来说清楚就是：`wc_url` 去掉仓库根剩下一段前缀，日志路径带着这段前缀时，
/// 后面的部分拼到 `local` 上就是本地文件。前缀对不上返回 `None`——那次提交动的是
/// 同一个仓库里的别的项目，本地这个目录里没有它。
pub fn wc_target(repos_root: &str, wc_url: &str, local: &Path, repo_path: &str) -> Option<PathBuf> {
    let root = repos_root.trim_end_matches('/');
    let url = wc_url.trim_end_matches('/');
    if root.is_empty() || url.is_empty() || local.as_os_str().is_empty() {
        return None;
    }
    if url.len() < root.len() || !url[..root.len()].eq_ignore_ascii_case(root) {
        return None;
    }
    // 本目录在仓库里的位置（相对仓库根，不带首斜杠）
    let here = url[root.len()..].trim_start_matches('/');
    let want = repo_path.trim_start_matches('/');
    let rest = if here.is_empty() {
        want
    } else if want.eq_ignore_ascii_case(here) {
        ""
    } else if want.len() > here.len()
        && want[..here.len()].eq_ignore_ascii_case(here)
        && want.as_bytes()[here.len()] == b'/'
    {
        &want[here.len() + 1..]
    } else {
        return None;
    };
    if rest.is_empty() {
        return Some(local.to_path_buf());
    }
    Some(local.join(rest.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

// ------------------------------------------------------------------------ svn 客户端

#[derive(Clone, Debug, Default)]
pub struct Svn {
    pub exe: PathBuf,
    pub username: String,
    pub password: String,
}

/// 忽略空白与换行差异时给 `svn diff` 追加的参数。两个值必须整体作为 `-x` 的一个参数
/// 交给 svn 内置的 diff 引擎（所以两条取差异的命令都固定带 `--internal-diff`）。
pub(crate) const DIFF_IGNORE_ARGS: [&str; 2] = ["-x", "-w --ignore-eol-style"];

/// 上面那组参数的一行写法，用于把命令回显到输出记录里。
pub(crate) fn diff_flags(ignore_white: bool) -> String {
    if ignore_white {
        format!(" {} \"{}\"", DIFF_IGNORE_ARGS[0], DIFF_IGNORE_ARGS[1])
    } else {
        String::new()
    }
}

impl Svn {
    pub fn available(&self) -> bool {
        !self.exe.as_os_str().is_empty() && self.exe.is_file()
    }

    pub fn version(&self) -> Option<String> {
        if !self.available() {
            return None;
        }
        let run = self.call(&["--version", "--quiet"], None);
        if run.ok {
            Some(run.out.trim().to_owned())
        } else {
            None
        }
    }

    fn global(&self) -> Vec<String> {
        let mut args = vec![
            "--non-interactive".to_owned(),
            "--trust-server-cert-failures=unknown-ca,cn-mismatch,expired,not-yet-valid,other"
                .to_owned(),
        ];
        if !self.username.trim().is_empty() {
            args.push("--username".to_owned());
            args.push(self.username.trim().to_owned());
            args.push("--password".to_owned());
            args.push(self.password.clone());
        }
        args
    }

    fn command(&self, args: &[String], cwd: Option<&Path>) -> Command {
        let mut command = Command::new(&self.exe);
        command.args(args);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        command
    }

    fn capture(&self, args: &[String], cwd: Option<&Path>) -> Run {
        match self
            .command(args, cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                // 挂进主程序的任务对象：程序退出时这条 svn 命令会被内核连带结束
                crate::childproc::attach(&child);
                match child.wait_with_output() {
                    Ok(output) => Run::new(
                        output.status.success(),
                        output.status.code().unwrap_or(-1),
                        decode_bytes(&output.stdout),
                        decode_bytes(&output.stderr),
                    ),
                    Err(e) => Run::error(format!("等待 {} 结束失败：{}", self.exe.display(), e)),
                }
            }
            Err(e) => Run::error(format!("无法启动 {}：{}", self.exe.display(), e)),
        }
    }

    /// 逐行回调执行（update / commit 等耗时命令）。
    fn stream(&self, args: &[String], cwd: Option<&Path>, on_line: &dyn Fn(&str)) -> Run {
        let mut child = match self
            .command(args, cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                // 同 capture：挂进任务对象，主程序退出时连带结束
                crate::childproc::attach(&child);
                child
            }
            Err(e) => return Run::error(format!("无法启动 {}：{}", self.exe.display(), e)),
        };
        let stderr = child.stderr.take();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr {
                let _ = pipe.read_to_end(&mut buf);
            }
            decode_bytes(&buf)
        });
        let mut all: Vec<u8> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0_u8; 8192];
        if let Some(mut pipe) = child.stdout.take() {
            while let Ok(size) = pipe.read(&mut chunk) {
                if size == 0 {
                    break;
                }
                all.extend_from_slice(&chunk[..size]);
                pending.extend_from_slice(&chunk[..size]);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    let text = decode_bytes(&line).trim_end_matches(['\n', '\r']).to_owned();
                    if !text.is_empty() {
                        on_line(&text);
                    }
                }
            }
        }
        if !pending.is_empty() {
            let text = decode_bytes(&pending).trim().to_owned();
            if !text.is_empty() {
                on_line(&text);
            }
        }
        let status = child.wait();
        let err = reader.join().unwrap_or_default();
        Run::new(
            status.as_ref().map(|s| s.success()).unwrap_or(false),
            status.ok().and_then(|s| s.code()).unwrap_or(-1),
            decode_bytes(&all),
            err,
        )
    }

    fn call(&self, args: &[&str], cwd: Option<&Path>) -> Run {
        let mut owned = self.global();
        owned.extend(args.iter().map(|a| (*a).to_owned()));
        self.capture(&owned, cwd)
    }

    fn call_stream(&self, args: &[&str], cwd: Option<&Path>, on_line: &dyn Fn(&str)) -> Run {
        let mut owned = self.global();
        owned.extend(args.iter().map(|a| (*a).to_owned()));
        self.stream(&owned, cwd, on_line)
    }

    pub fn info(&self, target: &str) -> (Option<WcInfo>, Run) {
        let run = self.call(&["info", "--xml", target], None);
        (
            if run.ok {
                parse_info(&run.out)
            } else {
                None
            },
            run,
        )
    }

    pub fn status(&self, dir: &Path) -> (Vec<StatusEntry>, Run) {
        let run = self.call(&["status", "--xml", &dir.to_string_lossy()], None);
        (
            if run.ok {
                parse_status(&run.out, dir)
            } else {
                Vec::new()
            },
            run,
        )
    }

    /// 一次走完整棵树同时读出两件事：本地未提交的改动（`wc-status`）和服务器上已变更、
    /// 本地还没更新的条目（`repos-status`）。早先分 `status` + `status -u` 两条命令等于
    /// 把树走两遍——七万文件的工作副本要多等三秒。要走网络，只在后台检测里调。
    /// 可更新数返回 None 表示这次没读到（断网 / 认证异常），调用方不能据此断定「已是最新」。
    pub fn status_against_server(&self, dir: &Path) -> (Vec<StatusEntry>, Option<usize>, Run) {
        let run = self.call(&["status", "-u", "--xml", &dir.to_string_lossy()], None);
        if !run.ok {
            return (Vec::new(), None, run);
        }
        (
            parse_status(&run.out, dir),
            Some(parse_out_of_date(&run.out)),
            run,
        )
    }

    /// 提交记录一律读服务器：不带 `-r HEAD:1` 时，在工作副本里执行 `svn log`
    /// 只读到本地 BASE（本地 r6608、服务器已经 r6610 时，服务器上新的记录就看不见）。
    /// `author` 非空时再加 `--search`，让 svn 只回这个人的提交，少传一堆数据。
    /// `target` 可以是工作副本路径，也可以是仓库 URL（单个文件的修改记录就用 URL 查，
    /// 这样即使该文件在本地已被改名 / 删除也能读到记录）。
    /// `limit` > 0 时加 `-l` 限制条数；<= 0 表示不限条数（不带 `-l`，svn 会拉全量）。
    pub fn log(&self, target: &str, limit: i64, author: &str) -> (Vec<LogEntry>, Run) {
        self.log_in(target, "HEAD:1", limit, author)
    }

    /// 同 `log`，但由调用方给出版本区间（`revspec`，如 `HEAD:1` 或 `{2026-09-11}:{2026-09-04}`）。
    /// 按日期区间读提交记录时必须走这里：`log` 固定的 `HEAD:1` 无法限定区间。
    /// `revspec` 一律写成「新 → 旧」：`-r` 是正序时 `-l` 会保留区间里**最旧**的 N 条，
    /// 所以日期区间这里不限制条数（`limit` 传 0），只靠区间本身收口。
    pub fn log_in(&self, target: &str, revspec: &str, limit: i64, author: &str) -> (Vec<LogEntry>, Run) {
        let mut args: Vec<&str> = vec!["log", "--xml", "-v", "-r", revspec];
        let count;
        if limit > 0 {
            count = limit.to_string();
            args.extend(["-l", &count]);
        }
        args.push(target);
        // 老版本 svn.exe 不认 --search 时的回退长度：截掉后面的 --search 参数
        let base_len = args.len();
        if !author.is_empty() {
            args.extend(["--search", author]);
        }
        let mut run = self.call(&args, None);
        if !run.ok && !author.is_empty() {
            // 老版本 svn.exe 不认 --search：退回不过滤，靠下面的作者过滤兜底
            args.truncate(base_len);
            run = self.call(&args, None);
        }
        let mut entries = if run.ok {
            parse_log(&run.out)
        } else {
            Vec::new()
        };
        if !author.is_empty() {
            entries.retain(|entry| entry.author.eq_ignore_ascii_case(author));
        }
        (entries, run)
    }

    /// 本机 svn 缓存的登录人（`svn auth`）。给出仓库地址时优先返回同一台服务器上的账号。
    pub fn auth_user(&self, url: &str) -> Option<String> {
        let run = self.call(&["auth"], None);
        if !run.ok {
            return None;
        }
        let want = url
            .split("//")
            .nth(1)
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let mut kind = String::new();
        let mut realm = String::new();
        let mut first = String::new();
        for line in run.out.lines() {
            let text = line.trim();
            if let Some(value) = text.strip_prefix("Credential kind:") {
                kind = value.trim().to_owned();
                realm.clear();
            } else if let Some(value) = text.strip_prefix("Authentication realm:") {
                realm = value.trim().to_lowercase();
            } else if let Some(value) = text.strip_prefix("Username:") {
                let user = value.trim().to_owned();
                if user.is_empty() || kind != "svn.simple" {
                    continue;
                }
                if first.is_empty() {
                    first = user.clone();
                }
                // realm 形如 "<https://192.168.1.251:443> visualsvn server"
                if !want.is_empty() && realm.contains(&want) {
                    return Some(user);
                }
            }
        }
        (!first.is_empty()).then_some(first)
    }

    pub fn update(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.call_stream(&["update", &dir.to_string_lossy()], None, on_line)
    }

    /// 组装取差异的参数：`head` 是 `svn diff` 的固定部分，`target` 是本地路径或仓库 URL。
    /// 一律带 `--internal-diff`，这样 `-x` 的参数必然由 svn 内置 diff 解释。
    /// 忽略空白与换行由调用方按当前这一项决定（XML 自动忽略，其余默认不忽略）。
    fn diff_args<'a>(&self, head: &[&'a str], target: &'a str, ignore_white: bool) -> Vec<&'a str> {
        let mut args: Vec<&'a str> = head.to_vec();
        if ignore_white {
            args.extend_from_slice(&DIFF_IGNORE_ARGS);
        }
        args.push(target);
        args
    }

    /// 单个文件的本次修改内容。
    pub fn diff(&self, path: &Path, ignore_white: bool) -> Run {
        let target = path.to_string_lossy().into_owned();
        let args = self.diff_args(&["diff", "--internal-diff"], &target, ignore_white);
        self.call(&args, None)
    }

    /// 某次提交对这个路径的逐行差异（和它的上一个版本比）：`svn diff -c <rev> <URL>`。
    /// 用仓库 URL 查，本地没有该文件 / 已被改名删除也能取到；本次新增的文件会整份显示为 +。
    pub fn diff_rev(&self, revision: &str, target: &str, ignore_white: bool) -> Run {
        let args = self.diff_args(&["diff", "-c", revision, "--internal-diff"], target, ignore_white);
        self.call(&args, None)
    }

    /// 取工作副本 BASE 版本的原始内容写到 `to`，供 Beyond Compare 之类外部工具对比。
    /// `svn cat -r BASE` 读的是本地 pristine 存储，不走网络。
    pub fn cat_base(&self, path: &Path, to: &Path) -> Result<(), String> {
        let bytes = self
            .cat_bytes("BASE", path)?
            .ok_or_else(|| format!("{} 没有 BASE 版本", path.display()))?;
        fs::write(to, bytes).map_err(|e| e.to_string())
    }

    /// `svn cat -r <revspec> <path>` 的**字节**版。`capture` 经 `decode_bytes` 是有损解码，
    /// 定岔要按字节比祖先，不能走那条。
    /// `Ok(None)` = 这一版上没有这个文件（文件比它晚才出现，或工作副本里它还没有 BASE），
    /// `Err` = 命令真失败了（连不上服务器、认证被拒）。
    /// 这个分界决定定岔是「跳过这一版继续往回走」还是「整轮判成没连上服务器」，
    /// 判错方向会把一次网络故障说成「这个文件没有基线」。
    pub(crate) fn cat_bytes(&self, revspec: &str, path: &Path) -> Result<Option<Vec<u8>>, String> {
        self.cat_bytes_limited(revspec, path, u64::MAX)
    }

    fn cat_bytes_limited(&self, revspec: &str, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
        let mut args = self.global();
        args.extend([
            "cat".to_owned(),
            "-r".to_owned(),
            revspec.to_owned(),
            path.to_string_lossy().into_owned(),
        ]);
        let (status, bytes, stderr) = self.raw_limited(&args, limit)?;
        if status.success() { return Ok(Some(bytes)) }
        let err = decode_bytes(&stderr);
        if says_missing(&err) { Ok(None) } else { Err(err.trim().to_owned()) }
    }

    fn raw_limited(&self, args: &[String], limit: u64) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
        let mut child = match self
            .command(args, None)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return Err(format!("无法启动 {}：{}", self.exe.display(), e)),
        };
        crate::childproc::attach(&child);
        let stderr = child.stderr.take().ok_or("无法读取 SVN 错误输出")?;
        let errors = std::thread::spawn(move || {
            let mut stderr = stderr;
            let mut bytes = Vec::new();
            let result = (&mut stderr).take(1024 * 1024).read_to_end(&mut bytes);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            result.map(|_| bytes)
        });
        let mut bytes = Vec::new();
        let read = child.stdout.take().ok_or("无法读取 SVN 文件内容")?.take(limit.saturating_add(1)).read_to_end(&mut bytes);
        if read.is_err() || bytes.len() as u64 > limit { let _ = child.kill(); }
        let status = child.wait().map_err(|e| format!("等待 SVN 结束失败：{e}"))?;
        let stderr = errors.join().map_err(|_| "读取 SVN 错误输出失败")?.map_err(|e| e.to_string())?;
        read.map_err(|e| e.to_string())?;
        if bytes.len() as u64 > limit { return Err(format!("命令输出超过读取上限 {limit} 字节")); }
        Ok((status, bytes, stderr))
    }


    pub fn cleanup(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.call_stream(&["cleanup", &dir.to_string_lossy()], None, on_line)
    }

    pub fn revert_all(&self, dir: &Path, on_line: &dyn Fn(&str)) -> Run {
        self.call_stream(
            &["revert", "--recursive", &dir.to_string_lossy()],
            None,
            on_line,
        )
    }

    /// 版本回退：把 `target`（一个目录或单个文件）倒回 `revision` 那个版本的样子。
    /// 产物是一批**本地改动**（M / A / D），不提交就不动服务器，不想要了用「撤销修改」还原。
    /// 锚点写 `HEAD` 而不是 `BASE`：svn 把范围收拢到目标当前所在的那个版本，所以工作副本
    /// 落后于服务器时也一样正好回到 `revision`（拿临时仓库实测：本地停在 r2、仓库 HEAD 是 r3，
    /// `merge -r HEAD:1` 报的是「Reverse-merging r2」）。
    /// `--accept postpone`：这一趟没给它终端，回退自己在同一行上撞出来的冲突不能让 svn 停下来
    /// 等键盘；留下 `.merge-left / .merge-right / .working` 三件套，交给「解决冲突」或 BC 手工合。
    /// 同一路径自己往回合不会留下 `svn:mergeinfo`（svn 记完就 elide 掉了），属性不会被带脏。
    pub fn merge_back(&self, target: &Path, revision: &str, on_line: &dyn Fn(&str)) -> Run {
        let owned = Self::merge_back_args(&target.to_string_lossy(), revision);
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        self.call_stream(&args, None, on_line)
    }

    /// `merge -r HEAD:<rev>` 的参数。同一条路径要写两遍：svn 的位置参数是「合并源 合并目标」，
    /// 只给一条时那条被当成合并源、目标退回进程当前目录 —— 主程序从工作副本外面启动时
    /// 整条命令直接 E155007（拿临时仓库实测）。路径当合并源用自己的 URL，不必先问服务器。
    pub fn merge_back_args(target: &str, revision: &str) -> Vec<String> {
        vec![
            "merge".to_owned(),
            "-r".to_owned(),
            format!("HEAD:{revision}"),
            "--accept".to_owned(),
            "postpone".to_owned(),
            target.to_owned(),
            target.to_owned(),
        ]
    }

    /// 「解决冲突」窗口按选定策略收场：同一条策略的多个路径并成一条命令发给 svn。
    /// `--recursive` 是为了目录条目（tree conflict 挂在目录上时不带它解决不掉），
    /// 对单个文件没有副作用。
    pub fn accept_conflicts(
        &self,
        accept: Accept,
        targets: &[PathBuf],
        on_line: &dyn Fn(&str),
    ) -> Run {
        let mut owned: Vec<String> = match accept.flag() {
            Some(flag) => vec!["resolve".to_owned(), "--accept".to_owned(), flag.to_owned()],
            None => vec!["revert".to_owned()],
        };
        owned.push("--recursive".to_owned());
        owned.extend(targets.iter().map(|t| t.to_string_lossy().into_owned()));
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        self.call_stream(&args, None, on_line)
    }

    /// 修改仓库地址（`svn relocate`）：只改写工作副本里的 URL 元数据，不动任何本地文件。
    /// `from` / `to` 是地址前缀，svn 会把工作副本 URL 中匹配到的 `from` 换成 `to`；
    /// svn 会连新地址一起校验仓库 UUID，地址写错只是失败，不会把工作副本改坏。
    pub fn relocate(&self, dir: &Path, from: &str, to: &str, on_line: &dyn Fn(&str)) -> Run {
        let target = dir.to_string_lossy().into_owned();
        self.call_stream(&["relocate", from, to, &target], None, on_line)
    }

    /// 目标过多时改用 `--targets` 文件（按本机 ANSI 编码写入，中文系统即 GBK）。
    fn with_targets(&self, head: Vec<String>, targets: &[String]) -> Vec<String> {
        let width: usize = targets.iter().map(|t| t.chars().count() + 3).sum();
        let mut args = head;
        if width > 12_000 {
            let file = temp_file("repo_manager_targets");
            let body = targets.join("\r\n");
            if fs::write(&file, GBK.encode(&body).0.as_ref()).is_ok() {
                args.push("--targets".to_owned());
                args.push(file.to_string_lossy().into_owned());
                return args;
            }
        }
        args.extend(targets.iter().cloned());
        args
    }

    pub fn add(&self, dir: &Path, targets: &[String], on_line: &dyn Fn(&str)) -> Run {
        let args = self.with_targets(vec!["add".to_owned(), "--force".to_owned()], targets);
        self.stream(&args, Some(dir), on_line)
    }

    pub fn delete(&self, dir: &Path, targets: &[String], on_line: &dyn Fn(&str)) -> Run {
        let args = self.with_targets(vec!["delete".to_owned()], targets);
        self.stream(&args, Some(dir), on_line)
    }

    pub fn commit(
        &self,
        dir: &Path,
        targets: &[String],
        message: &str,
        on_line: &dyn Fn(&str),
    ) -> Run {
        let message_file = temp_file("repo_manager_log");
        if fs::write(&message_file, message.as_bytes()).is_err() {
            return Run::error("无法写入提交说明临时文件");
        }
        let head = self.with_targets(
            vec!["commit".to_owned(), "--depth".to_owned(), "empty".to_owned()],
            targets,
        );
        let mut args = self.global();
        args.extend(head);
        args.push("--file".to_owned());
        args.push(message_file.to_string_lossy().into_owned());
        args.push("--encoding".to_owned());
        args.push("UTF-8".to_owned());
        let run = self.stream(&args, Some(dir), on_line);
        let _ = fs::remove_file(&message_file);
        run
    }
}

// --------------------------------------------------------------- 合并用的祖先来源

/// `svn cat` 报「这一版上没有这个文件」的几种原文。错误码随 svn 版本与命令路径而变，
/// 认不出来的一律当命令失败：把一次网络故障误判成「没有基线」会让报告指错处置方向。
fn says_missing(err: &str) -> bool {
    let text = err.to_lowercase();
    [
        "e160013",
        "e200009",
        "not found at revision",
        "not found in revision",
        "does not exist in",
        "non-existent in",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// `merge::Ancestor` 的 svn 实现。
///
/// 状态表取自构造时那一次 `svn status --xml <root>`：svn 只把**非正常**条目列出来，
/// 所以「表里没有 + 盘上存在」就是正常版本化，「表里没有 + 盘上也没有」就是不在工作副本里。
/// 整棵树只走这一遍，几百个文件共用一张表。
/// `status` 命令本身失败时（`error()` 给出原因），盘上的文件一概按「非常规状态」处理：
/// 宁可整片跳过，也不把内容写进一个可能正冲突、或已计划删除的文件。
pub struct SvnAncestor {
    svn: Svn,
    root: PathBuf,
    states: HashMap<String, WcState>,
    failed: Option<String>,
    property_modified: Vec<String>,
}

impl SvnAncestor {
    pub fn new_local(svn: Svn, root: &Path) -> Self {
        let mut anc = Self::new(svn, root);
        // status 对普通目录可能以成功退出、仅输出 warning，不能仅靠退出码确认版本化。
        let (info, run) = anc.svn.info(&root.to_string_lossy());
        if info.is_some() && run.ok {
            return anc;
        }
        let reason = format!("{}\n{}", run.out, run.err);
        // 只将明确的“不是工作副本/节点未版本化”识别为普通目录；认证、权限等错误仍保留。
        if root.is_dir() && ["E155007", "W155007", "W155010"].iter().any(|code| reason.contains(code)) {
            anc.failed = None;
            anc.states.clear();
            anc.states.insert(String::new(), WcState::Unversioned);
        } else {
            anc.failed = Some(run.summary());
        }
        anc
    }

    pub fn new(svn: Svn, root: &Path) -> Self {
        // Ignored directories must be represented too: otherwise their children
        // look like normal versioned files and may be mistaken for deletions.
        let run = svn.call(&["status", "--xml", "--no-ignore", &root.to_string_lossy()], None);
        let entries = if run.ok { parse_status(&run.out, root) } else { Vec::new() };
        let failed = if !run.ok || run.err.contains("W155007") {
            Some(run.summary())
        } else {
            roxmltree::Document::parse(&run.out).err().map(|e| format!("工作副本状态 XML 无效：{e}"))
        };
        let mut states = state_table(&entries, root);
        if let Ok(doc) = roxmltree::Document::parse(&run.out) {
            for entry in doc.descendants().filter(|n| n.has_tag_name("entry")) {
                if entry.children().any(|n| n.has_tag_name("wc-status") && n.attribute("switched") == Some("true")) {
                    if let Some(raw) = entry.attribute("path") {
                        let path = Path::new(raw);
                        let rel = path.strip_prefix(root).unwrap_or(path);
                        let key = rel_key(rel);
                        // 绑定目录本身的 URL 由 binding 核对；内部 switched 子树需先人工处理。
                        if !key.is_empty() { states.insert(key, WcState::Other); }
                    }
                }
            }
        }
        Self {
            states,
            root: root.to_path_buf(),
            failed,
            property_modified: entries.iter().filter(|e| matches!(e.props.as_str(), "modified" | "conflicted"))
                .map(|e| rel_key(Path::new(&e.path).strip_prefix(root).unwrap_or(Path::new(&e.path)))).collect(),
            svn,
        }
    }

    /// 状态表没读出来时给调用方的原因：这个功能整个跑不起来，别一边跑一边报「无基线」
    pub fn error(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// 状态表按「小写 + `/` 分隔的相对路径」存，和 `localdiff` 的 rel 同一坐标系；
/// svn 只把非正常条目列出来，所以这张表是一份覆盖用的例外清单
fn state_table(entries: &[StatusEntry], root: &Path) -> HashMap<String, WcState> {
    let mut states = HashMap::new();
    for entry in entries {
        let path = Path::new(&entry.path);
        let rel = path.strip_prefix(root).unwrap_or(path);
        let key = rel_key(rel);
        let state = if entry.props == "conflicted" { WcState::Conflict } else { state_of(entry.item) };
        if !key.is_empty() || state != WcState::Normal {
            states.insert(key, state);
        }
    }
    states
}

fn rel_key(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// svn 的条目状态映射到合并功能的处置口径
fn state_of(item: Item) -> WcState {
    match item {
        Item::Normal | Item::Modified => WcState::Normal,
        Item::Added => WcState::Added,
        Item::Unversioned => WcState::Unversioned,
        Item::Deleted | Item::Missing => WcState::Deleted,
        Item::Conflict => WcState::Conflict,
        Item::Replaced | Item::Ignored | Item::External | Item::Incomplete | Item::Other => WcState::Other,
    }
}

impl Ancestor for SvnAncestor {
    fn metadata(&self) -> Result<Option<crate::merge::Metadata>, String> {
        if let Some(reason) = &self.failed { return Err(reason.clone()) }
        if self.states.get("") == Some(&WcState::Unversioned) { return Ok(None) }
        let mut args = self.svn.global();
        args.extend(["proplist".into(), "--xml".into(), "--verbose".into(), "--recursive".into(), self.root.to_string_lossy().into_owned()]);
        let (status, raw, stderr) = self.svn.raw_limited(&args, 16 * 1024 * 1024)?;
        if !status.success() { return Err(decode_bytes(&stderr)) }
        let text = std::str::from_utf8(&raw).map_err(|e| e.to_string())?;
        let doc = roxmltree::Document::parse(text).map_err(|e| format!("SVN 属性 XML 无效：{e}"))?;
        let mut metadata = crate::merge::Metadata::new();
        for target in doc.descendants().filter(|n| n.has_tag_name("target")) {
            let path = Path::new(target.attribute("path").ok_or("SVN 属性缺少路径")?);
            let key = rel_key(path.strip_prefix(&self.root).unwrap_or(path));
            let mut properties = std::collections::BTreeMap::new();
            for prop in target.children().filter(|n| n.has_tag_name("property")) {
                let name = prop.attribute("name").ok_or("SVN 属性缺少名称")?;
                properties.insert(name.to_owned(), format!("{}:{}", prop.attribute("encoding").unwrap_or("text"), prop.text().unwrap_or("")));
            }
            if !properties.is_empty() { metadata.insert(key, properties); }
        }
        Ok(Some(metadata))
    }

    fn pending_properties(&self) -> Vec<String> { self.property_modified.clone() }

    fn binding(&self) -> Result<Option<crate::merge::RepositoryBinding>, String> {
        if let Some(reason) = &self.failed { return Err(reason.clone()) }
        if self.states.get("") == Some(&WcState::Unversioned) { return Ok(None) }
        let run = self.svn.call(&["info", "--xml", &self.root.to_string_lossy()], None);
        if !run.ok { return Err(run.summary()) }
        let doc = roxmltree::Document::parse(&run.out).map_err(|e| format!("SVN 身份信息无效：{e}"))?;
        let uuid = first_text(&doc, "uuid");
        let url = first_text(&doc, "url");
        if uuid.is_empty() || url.is_empty() { return Err("SVN 身份缺少仓库 UUID 或 URL".into()) }
        Ok(Some(crate::merge::RepositoryBinding { uuid, url }))
    }

    fn validation_client(&self) -> Option<Svn> { Some(self.svn.clone()) }

    fn wc_state(&self, rel: &str, exists: bool) -> WcState {
        if self.failed.is_some() {
            return WcState::Other;
        }
        let key = rel_key(Path::new(rel));
        let mut current = key.as_str();
        let mut found = None;
        loop {
            if let Some(state) = self.states.get(current) {
                // SVN may report only the directory for unversioned, missing,
                // deleted or conflicted subtrees. Its children inherit that state.
                if !state.writable() {
                    return *state;
                }
                if found.is_none() && (current == key || *state != WcState::Normal) {
                    found = Some(*state);
                }
            }
            if current.is_empty() {
                break;
            }
            current = current.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
        }
        found.unwrap_or(if exists { WcState::Normal } else { WcState::Absent })
    }

    fn base(&self, rel: &str) -> Result<Option<Vec<u8>>, String> {
        self.svn.cat_bytes_limited("BASE", &self.path(rel), crate::merge::MAX_FILE_BYTES)
    }

    /// `-r BASE:1` 是 svn 对工作副本路径的默认区间，写成「新 → 旧」才能让 `-l` 留下最近的那几版
    /// （正序时 `-l` 保留的是区间里最旧的 N 条）。第一条就是 BASE 内容来自的那一版，
    /// 调用方已经比过 BASE，再 cat 一次只是白花一趟网络往返 ⇒ 丢掉。
    fn revs(&self, rel: &str, max: usize) -> Result<Vec<u64>, String> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let target = self.path(rel).to_string_lossy().into_owned();
        let (log, run) = self.svn.log_in(&target, "BASE:1", max as i64 + 1, "");
        if !run.ok {
            return Err(run.summary());
        }
        Ok(log
            .iter()
            .filter_map(|entry| entry.revision.trim().parse::<u64>().ok())
            .skip(1)
            .take(max)
            .collect())
    }

    fn at_rev(&self, rel: &str, rev: u64) -> Result<Option<Vec<u8>>, String> {
        self.svn.cat_bytes_limited(&rev.to_string(), &self.path(rel), crate::merge::MAX_FILE_BYTES)
    }

}

// ------------------------------------------------------------------------ 自动寻找 svn.exe

pub(crate) fn add_path(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

pub(crate) fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

pub(crate) fn drive_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = (b'C'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .collect();
    roots.retain(|root| root.exists());
    roots.sort_by_key(|root| root.display().to_string());
    roots
}

const CLIENT_SUBDIRS: &[&str] = &[
    "TortoiseSVN/bin",
    "Subversion/bin",
    "SlikSvn/bin",
    "VisualSVN Subversion/bin",
    "VisualSVN Server/bin",
    "CollabNet/Subversion Client/bin",
    "WinSVN/bin",
    "Apache Subversion/bin",
    "subversion/bin",
    "svn/bin",
];

pub(crate) const SEARCH_BASES: &[&str] = &[
    "Program Files",
    "Program Files (x86)",
    "Program",
    "Program\\Tools",
    "Program Files\\Tools",
    "Tools",
    "Dev",
    "Soft",
    "Software",
    "usr",
    "opt",
];

fn registry_candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for key in [
        "HKLM\\SOFTWARE\\TortoiseSVN",
        "HKLM\\SOFTWARE\\WOW6432Node\\TortoiseSVN",
        "HKCU\\SOFTWARE\\TortoiseSVN",
    ] {
        let Ok(output) = Command::new("reg").args(["query", key]).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        for line in decode_bytes(&output.stdout).lines() {
            let Some(pos) = line.find("REG_SZ") else {
                continue;
            };
            let value = line[pos + "REG_SZ".len()..].trim();
            if value.is_empty() || !value.contains(':') {
                continue;
            }
            let base = PathBuf::from(value);
            for candidate in [base.join("bin").join("svn.exe"), base.join("svn.exe")] {
                if candidate.is_file() {
                    add_path(&mut found, candidate);
                }
            }
        }
    }
    found
}

fn known_locations() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(custom) = env_path("REPO_MANAGER_SVN") {
        add_path(&mut found, custom);
    }
    for name in ["SVN_ROOT", "SVN_HOME", "Subversion_Home"] {
        if let Some(root) = env_path(name) {
            add_path(&mut found, root.join("bin").join("svn.exe"));
            add_path(&mut found, root.join("svn.exe"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            add_path(&mut found, dir.join("svn.exe"));
        }
    }
    for name in [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "LOCALAPPDATA",
        "ProgramData",
    ] {
        let Some(base) = env_path(name) else {
            continue;
        };
        for sub in CLIENT_SUBDIRS {
            add_path(&mut found, base.join(sub).join("svn.exe"));
        }
    }
    if let Some(profile) = env_path("USERPROFILE") {
        for sub in [
            "scoop\\apps\\subversion\\current\\bin",
            "scoop\\apps\\tortoisesvn\\current\\bin",
            "AppData\\Local\\Programs\\TortoiseSVN\\bin",
        ] {
            add_path(&mut found, profile.join(sub).join("svn.exe"));
        }
    }
    if let Some(programdata) = env_path("ProgramData") {
        add_path(&mut found, programdata.join("chocolatey\\bin\\svn.exe"));
    }
    for root in drive_roots() {
        for base in SEARCH_BASES {
            let dir = root.join(base);
            for sub in CLIENT_SUBDIRS {
                add_path(&mut found, dir.join(sub).join("svn.exe"));
            }
        }
    }
    found.extend(registry_candidates());
    found.retain(|path| path.is_file());
    found
}

fn scan_dir(dir: &Path, depth: usize, hits: &mut Vec<PathBuf>, start: &Instant) {
    if depth == 0 || hits.len() >= 24 || start.elapsed() > Duration::from_secs(6) {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        if start.elapsed() > Duration::from_secs(6) {
            return;
        }
        let name = item.file_name().to_string_lossy().to_lowercase();
        if [
            "windows",
            "$recycle.bin",
            "system volume information",
            "perflogs",
            "recovery",
            "users",
            "appdata",
            "programdata",
            "temp",
        ]
        .iter()
        .any(|skip| name.starts_with(skip))
        {
            continue;
        }
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let child = item.path();
        if name.contains("svn") {
            let candidate = child.join("bin").join("svn.exe");
            if candidate.is_file() {
                add_path(hits, candidate);
            }
        }
        scan_dir(&child, depth - 1, hits, start);
    }
}

/// 所有可用的 svn.exe 候选路径（按可信度排序）。
pub fn candidates() -> Vec<PathBuf> {
    let mut found = known_locations();
    if found.is_empty() {
        // 常见位置都没有时才做磁盘浅层扫描（最慢的一步，限时 6 秒）
        let start = Instant::now();
        let mut hits = Vec::new();
        for root in drive_roots() {
            scan_dir(&root, 3, &mut hits, &start);
        }
        hits.sort_by_key(|path| path.components().count());
        for hit in hits {
            add_path(&mut found, hit);
        }
    }
    found
}

/// 自动寻找并验证 svn.exe，返回 (路径, 版本号)。
pub fn detect(list: &[PathBuf]) -> Option<(PathBuf, String)> {
    for path in list {
        let svn = Svn {
            exe: path.clone(),
            ..Default::default()
        };
        if let Some(version) = svn.version() {
            return Some((path.clone(), version));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Svn {
        Svn {
            exe: PathBuf::from("svn"),
            ..Default::default()
        }
    }

    /// 「全部上传」的口径：A/M/D/R 直接可提交，?（自动 add）和 !（自动 delete）也算；
    /// 冲突、不完整、已忽略、外部引用不能自动带上服务器。
    #[test]
    fn uploadable_covers_pending_add_and_delete() {
        let yes = [
            Item::Added,
            Item::Modified,
            Item::Deleted,
            Item::Replaced,
            Item::Unversioned,
            Item::Missing,
        ];
        let no = [
            Item::Conflict,
            Item::Incomplete,
            Item::Ignored,
            Item::External,
            Item::Normal,
            Item::Other,
        ];
        for item in yes {
            assert!(item.uploadable(), "{} 应算待提交", item.mark());
        }
        for item in no {
            assert!(!item.uploadable(), "{} 不应自动提交", item.mark());
        }
    }

    /// status 刷新后的默认勾选口径 = uploadable：?（自动 add）、!（自动 delete）和
    /// A/M/D/R 一样进场就勾上；冲突这类必须人工处理的依旧不勾。
    #[test]
    fn parse_status_checks_everything_uploadable_by_default() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc\mod.txt"><wc-status item="modified" props="none"/></entry>
    <entry path="D:\wc\new.txt"><wc-status item="unversioned" props="none"/></entry>
    <entry path="D:\wc\gone.txt"><wc-status item="missing" props="none"/></entry>
    <entry path="D:\wc\clash.txt"><wc-status item="conflicted" props="none"/></entry>
  </target>
</status>"#;
        let entries = parse_status(xml, Path::new(r"D:\wc"));
        let checked = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("缺条目 {name}"))
                .checked
        };
        assert!(checked("mod.txt"), "修改项应默认勾选");
        assert!(checked("new.txt"), "未版本化(?)应默认勾选（提交时自动 add）");
        assert!(checked("gone.txt"), "已丢失(!)应默认勾选（提交时自动 delete）");
        assert!(!checked("clash.txt"), "冲突项必须先人工处理，不默认勾选");
    }

    /// 「可更新」只数带 <repos-status> 的条目：光比本地 Revision 和仓库 HEAD，会把
    /// 自己刚提交的文件误判成「有新版可更新」。
    #[test]
    fn out_of_date_counts_repos_status_entries() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc"><wc-status item="normal" props="none" revision="1097"/></entry>
    <entry path="D:\wc\a.vue"><wc-status item="normal" props="none" revision="1097"/><repos-status item="modified" props="none"/></entry>
    <entry path="D:\wc\b.vue"><wc-status item="normal" props="none" revision="1097"/><repos-status item="modified" props="none"/></entry>
  </target>
  <against revision="1098"/>
</status>"#;
        assert_eq!(parse_out_of_date(xml), 2, "只有带 repos-status 的条目算待更新");
        assert_eq!(parse_out_of_date("<status>"), 0, "输出残缺时不误报");
    }

    /// 短路只允许「本地基线 == HEAD 就不问服务器」这一个方向，且两个版本号缺一不可：
    /// 读不到 HEAD（仓库断了）就照常走 `-u`，别把「不知道」报成「已是最新」。
    #[test]
    fn at_head_needs_both_revisions_and_exact_agreement() {
        assert!(wc_at_head("6700", "6700"), "基线就是 HEAD，可以省掉 -u");
        assert!(
            !wc_at_head("6700", "6713"),
            "落后于 HEAD 时必须走 -u 把可更新项数清楚"
        );
        assert!(!wc_at_head("", "6700"), "本地版本号没读到不算已是最新");
        assert!(!wc_at_head("6700", ""), "HEAD 没读到（仓库连不上）同样不能短路");
    }

    /// 合并读取的前提：同一份 `svn status -u --xml` 输出要能同时给出「本地待提交数」
    /// 和「服务器可更新数」，这样检测时不必把整棵工作副本树走两遍。
    #[test]
    fn one_status_u_read_yields_both_counts() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc\a.vue"><wc-status item="modified" props="none" revision="1097"/></entry>
    <entry path="D:\wc\note.txt"><wc-status item="unversioned" props="none"/></entry>
    <entry path="D:\wc\clash.vue"><wc-status item="conflicted" props="none" revision="1097"/></entry>
    <entry path="D:\wc\b.vue"><wc-status item="normal" props="none" revision="1097"/><repos-status item="modified" props="none"/></entry>
  </target>
  <against revision="1098"/>
</status>"#;
        let entries = parse_status(xml, Path::new(r"D:\wc"));
        assert_eq!(entries.len(), 4, "四个条目都该收到");
        // 口径同「全部上传」：修改 + 未版本控制算待提交，冲突要先人工处理
        let changes = entries.iter().filter(|e| e.item.uploadable()).count();
        assert_eq!(changes, 2, "待提交数不该把冲突或 normal 条目算进来");
        assert_eq!(blocked_count(&entries), 1, "冲突单独数一处");
        assert_eq!(parse_out_of_date(xml), 1, "只有带 repos-status 的那条算可更新");
    }

    /// 目录行「冲突 N」与提交页「需处理」共用 blocked_count，口径只能有一处。
    #[test]
    fn blocked_count_covers_conflicts_and_incomplete_only() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc\a.java"><wc-status item="conflicted" props="none" revision="10"/></entry>
    <entry path="D:\wc\b.java"><wc-status item="conflicted" props="items" revision="10"/></entry>
    <entry path="D:\wc\c"><wc-status item="incomplete" props="none" revision="10"/></entry>
    <entry path="D:\wc\d.java"><wc-status item="modified" props="none" revision="10"/></entry>
    <entry path="D:\wc\e.java"><wc-status item="unversioned" props="none" revision="10"/></entry>
  </target>
</status>"#;
        let entries = parse_status(xml, Path::new(r"D:\wc"));
        assert_eq!(blocked_count(&entries), 3, "冲突两条 + 不完整一条");
        assert_eq!(blocked_count(&[]), 0);
        let clean = parse_status(
            r#"<status><target path="D:\wc"><entry path="D:\wc\a.java"><wc-status item="modified" props="none" revision="10"/></entry></target></status>"#,
            Path::new(r"D:\wc"),
        );
        assert_eq!(blocked_count(&clean), 0, "普通改动不算需处理");
    }

    #[test]
    fn working_copy_diff_uses_the_internal_diff() {
        let args = client().diff_args(&["diff", "--internal-diff"], r"D:\wc\a.java", false);
        assert_eq!(args, ["diff", "--internal-diff", r"D:\wc\a.java"]);
    }

    /// 忽略空白时 `-w --ignore-eol-style` 必须整体作为 `-x` 的一个参数交给内置 diff。
    #[test]
    fn ignore_white_adds_a_single_x_argument() {
        let args = client().diff_args(&["diff", "-c", "6511", "--internal-diff"], "http://x/a.xml", true);
        assert_eq!(
            args,
            [
                "diff",
                "-c",
                "6511",
                "--internal-diff",
                "-x",
                "-w --ignore-eol-style",
                "http://x/a.xml"
            ]
        );
        assert_eq!(diff_flags(true), " -x \"-w --ignore-eol-style\"");
        assert!(diff_flags(false).is_empty());
    }

    /// 单文件回退要靠它把日志里的仓库相对路径找回到本地文件上。
    #[test]
    fn log_paths_resolve_to_working_copy_files() {
        let root = "https://192.168.1.251/svn/java";
        let url = "https://192.168.1.251/svn/java/code/HRP.FA";
        let local = Path::new(r"D:\svn\code\HRP.FA");
        assert_eq!(
            wc_target(root, url, local, "/code/HRP.FA/HRP.FA.Bll/Sc/a.xml"),
            Some(PathBuf::from(r"D:\svn\code\HRP.FA\HRP.FA.Bll\Sc\a.xml"))
        );
        // 路径正好指到这个目录本身：回退目标就是本地目录
        assert_eq!(
            wc_target(root, url, local, "/code/HRP.FA"),
            Some(local.to_path_buf())
        );
        // 仓库根被整个检出时（本目录 = 仓库根）相对路径整段拼上去
        assert_eq!(
            wc_target(root, root, local, "/code/HRP.FA/a.cs"),
            Some(PathBuf::from(r"D:\svn\code\HRP.FA\code\HRP.FA\a.cs"))
        );
        // 那次提交还动了同仓库里的别的项目：本地这个目录里没有它，别乱拼
        assert_eq!(wc_target(root, url, local, "/code/HRP.His/a.cs"), None);
        assert_eq!(wc_target(root, url, local, "/code/HRP.FA.xml"), None);
        // 地址或本地路径没读到，同样拼不出来
        assert_eq!(wc_target("", url, local, "/code/HRP.FA/a.cs"), None);
        assert_eq!(wc_target(root, "", local, "/code/HRP.FA/a.cs"), None);
        assert_eq!(wc_target(root, url, Path::new(""), "/code/HRP.FA/a.cs"), None);
    }

    /// 四条收场策略发给 svn 的写法：三条是 `resolve --accept <取值>` 的不同取值，
    /// 「丢弃本地改动」resolve 里没有对应取值，只能另走 `revert`。
    #[test]
    fn accept_strategies_spell_their_svn_commands() {
        assert_eq!(Accept::Working.command(), "resolve --accept working");
        assert_eq!(Accept::MineFull.command(), "resolve --accept mine-full");
        assert_eq!(Accept::TheirsFull.command(), "resolve --accept theirs-full");
        assert_eq!(Accept::Revert.command(), "revert", "丢弃本地改动只能靠 revert");
        assert_eq!(Accept::default(), Accept::Working, "默认策略谁的一边都不丢");
        assert_eq!(Accept::ALL.len(), 4, "窗口下拉里的选项和这里的策略要一样多");
    }

    /// 「解决冲突」窗口列的条目必须就是目录行上数出来的那一批：两个口径只能有一处。
    #[test]
    fn blocked_entries_are_the_batch_blocked_count_counts() {
        let xml = r#"<status>
  <target path="D:\wc">
    <entry path="D:\wc\clash.txt"><wc-status item="conflicted" props="none"/></entry>
    <entry path="D:\wc\other\clash.txt"><wc-status item="conflicted" props="none"/></entry>
    <entry path="D:\wc\half.txt"><wc-status item="incomplete" props="none"/></entry>
    <entry path="D:\wc\mod.txt"><wc-status item="modified" props="none"/></entry>
    <entry path="D:\wc\new.txt"><wc-status item="unversioned" props="none"/></entry>
    <entry path="D:\wc\fine.txt"><wc-status item="normal" props="none"/></entry>
  </target>
</status>"#;
        let entries = parse_status(xml, Path::new(r"D:\wc"));
        let listed = blocked_entries(&entries);
        assert_eq!(listed.len(), blocked_count(&entries), "列出来的就该是数出来的那几条");
        assert_eq!(listed.len(), 3, "冲突两条 + 不完整一条，修改与未版本化不算");
        assert!(listed.iter().all(|e| e.item.blocked()), "不该掺进能提交的条目");
        assert_eq!(
            listed.iter().filter(|e| e.item == Item::Conflict).count(),
            2,
            "真冲突是「解决冲突」窗口能动手的那部分"
        );
        assert_eq!(listed[1].name, "clash.txt", "同名不同目录的两条都要留下，别按名字并掉");
    }

    /// 只写一遍路径时 svn 把它当合并源、目标取进程当前目录：主程序从工作副本外面点开就是
    /// 一条 E155007，回退根本发不出去（临时仓库实测）
    #[test]
    fn merge_back_names_both_the_source_and_the_target() {
        let path = r"D:\wc\trunk";
        let args = Svn::merge_back_args(path, "7");
        assert_eq!(
            args.join(" "),
            format!("merge -r HEAD:7 --accept postpone {path} {path}"),
            "路径要写两遍，一遍合并源一遍合并目标"
        );
    }

    /// 「这一版上没有这个文件」让定岔继续往回走，「命令失败」让整轮判成没连上服务器：
    /// 两者混在一起时，一次断网会被报成「这个文件没有基线」，处置方向完全错开
    #[test]
    fn a_missing_file_is_not_reported_as_a_failed_command() {
        for text in [
            "svn: E160013: File 'Foo.java' is not found at revision 200;",
            "svn: E160013: Foo.java does not exist in revision 190",
            "svn: E200009: Can't open 'Foo.java'",
        ] {
            assert!(says_missing(text), "{text} 说的是这一版上没有");
        }
        for text in [
            "svn: E170013: Unable to connect to a repository at URL 'https://10.0.0.1/svn/hrp'",
            "svn: E215004: No more credentials or we tried really bad ones twice",
            "",
        ] {
            assert!(!says_missing(text), "{text} 是命令失败，不能当成没有基线");
        }
    }

    /// 状态表是一份「例外清单」：svn 只列非正常条目，所以表里的状态要按归一化后的 rel 命中，
    /// 表外的要按「盘上有没有」分成正常版本化与不在工作副本里
    #[test]
    fn the_state_table_lists_only_what_svn_flagged() {
        let root = Path::new(r"D:\wc");
        let entry = |rel: &str, item: Item| StatusEntry {
            path: root.join(rel).display().to_string(),
            name: rel.to_owned(),
            item,
            props: "none".to_owned(),
            checked: false,
        };
        let states = state_table(
            &[
                entry(r"HRP.Web\build\Foo.java", Item::Conflict),
                entry("gone.txt", Item::Missing),
                entry("added.txt", Item::Added),
                entry("fresh.txt", Item::Unversioned),
                entry("ignored.txt", Item::Ignored),
                entry("", Item::Normal),
            ],
            root,
        );
        assert_eq!(states.len(), 5, "工作副本根目录自己那条不是一格");
        let anc = SvnAncestor {
            svn: client(),
            root: root.to_path_buf(),
            states,
            property_modified: Vec::new(), failed: None,
        };
        assert_eq!(
            anc.wc_state(r"HRP.Web\Build\FOO.JAVA", true),
            WcState::Conflict,
            "大小写与分隔符都不能挡住命中"
        );
        assert_eq!(anc.wc_state("gone.txt", true), WcState::Deleted, "已丢失按已计划删除处置");
        assert_eq!(anc.wc_state("ignored.txt", true), WcState::Other);
        assert_eq!(anc.wc_state("quiet.txt", true), WcState::Normal, "表里没有 + 盘上有 = 正常版本化");
        assert_eq!(anc.wc_state("fresh.txt", false), WcState::Unversioned, "表里的状态优先于盘上有没有");
        assert_eq!(anc.wc_state("newcomer.txt", false), WcState::Absent, "要拷进去的新目标");
    }

    /// 状态表根本没读出来时（这条目录不是工作副本、svn 报错），一个文件都不许判成可写
    #[test]
    fn an_unreadable_state_table_writes_nothing() {
        let anc = SvnAncestor {
            svn: client(),
            root: PathBuf::from(r"D:\nope"),
            states: HashMap::new(),
            property_modified: Vec::new(), failed: Some("svn: E155007: is not a working copy".to_owned()),
        };
        assert_eq!(anc.wc_state("Foo.java", true), WcState::Other);
        assert_eq!(anc.wc_state("Foo.java", false), WcState::Other);
        assert!(anc.error().is_some(), "读不到状态的原因要能交给上层说明");
    }

    #[test]
    fn directory_states_apply_to_descendants_without_matching_sibling_prefixes() {
        for state in [WcState::Deleted, WcState::Unversioned, WcState::Conflict, WcState::Other, WcState::Added] {
            let anc = SvnAncestor {
                svn: client(),
                root: PathBuf::from(r"D:\wc"),
                states: HashMap::from([("src/gone".to_owned(), state)]),
                property_modified: Vec::new(), failed: None,
            };
            assert_eq!(anc.wc_state(r"Src\Gone\nested\File.java", true), state);
            assert_eq!(anc.wc_state("src/gone/nested/file.java", false), state);
            assert_eq!(anc.wc_state("src/gone_other/file.java", true), WcState::Normal);
        }
    }

    #[test]
    fn an_unversioned_root_does_not_make_its_files_versioned() {
        let anc = SvnAncestor {
            svn: client(),
            root: PathBuf::from(r"D:\wc"),
            states: HashMap::from([("".to_owned(), WcState::Unversioned)]),
            property_modified: Vec::new(), failed: None,
        };
        assert_eq!(anc.wc_state("src/file.java", true), WcState::Unversioned);
    }

    #[test]
    fn a_tree_conflict_overrides_an_ordinary_child_status() {
        let root = Path::new(r"D:\wc");
        let xml = r#"<status><target path="D:\wc">
            <entry path="D:\wc\src"><wc-status item="normal" props="none" tree-conflicted="true"/></entry>
            <entry path="D:\wc\src\a.java"><wc-status item="modified" props="none"/></entry>
        </target></status>"#;
        let entries = parse_status(xml, root);
        let anc = SvnAncestor { svn: client(), root: root.to_path_buf(),
            states: state_table(&entries, root), property_modified: Vec::new(), failed: None };
        assert_eq!(anc.wc_state("src/a.java", true), WcState::Conflict);
    }

    #[test]
    fn a_root_property_conflict_blocks_children_and_replaced_nodes_need_review() {
        let root = Path::new(r"D:\wc");
        let entry = StatusEntry { path: root.display().to_string(), name: "wc".into(),
            item: Item::Normal, props: "conflicted".into(), checked: false };
        let anc = SvnAncestor { svn: client(), root: root.to_path_buf(), states: state_table(&[entry], root), property_modified: Vec::new(), failed: None };
        assert_eq!(anc.wc_state("src/a.java", true), WcState::Conflict);
        assert_eq!(state_of(Item::Replaced), WcState::Other);
    }
}
