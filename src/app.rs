//! 应用本体：状态构造、每帧的定时与拖放、后台任务结果的回收，以及底部输出面板。
//!
//! `RepoApp` 的结构体定义与共享类型留在 main.rs（crate 根），这样各模块照旧写 `crate::RepoApp`。

use crate::config;
use crate::{APP_TITLE, APP_VERSION, DirView, Level, OutLine, Page, RepoApp};
use crate::{bcompare, fonts, ink, pane_fill, pane_stroke, quiet, update};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use egui::{Align, Layout, RichText, ScrollArea, Ui};
use crate::config::Config;
use crate::jobs::{Data, Kind, Pool};
use crate::svn::Svn;

impl RepoApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = Config::load();
        let font_note = fonts::note_for(&cfg.ui_font, fonts::install(&cc.egui_ctx, &cfg.ui_font));
        let accent = {
            let [r, g, b] = crate::ui::accent_rgb(&cfg.accent);
            egui::Color32::from_rgb(r, g, b)
        };
        for style_theme in [egui::Theme::Dark, egui::Theme::Light] {
            cc.egui_ctx
                .style_mut_of(style_theme, |style| crate::ui::tune_style(style, accent));
        }
        // 直接 with_maximized 会让首帧画在旧尺寸上且不重绘，改为第一帧后再最大化
        cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        let version = {
            let svn = Svn {
                exe: PathBuf::from(&cfg.svn_exe),
                ..Default::default()
            };
            svn.version()
        };
        let svn = Svn {
            exe: PathBuf::from(&cfg.svn_exe),
            username: cfg.auth_user.clone(),
            password: cfg.auth_pass.clone(),
        };
        let saved_bc = PathBuf::from(&cfg.bc_exe);
        let bc = if saved_bc.is_file() {
            saved_bc
        } else {
            bcompare::detect().unwrap_or_default()
        };
        // git.exe：配置里有路径就用，留空则按 PATH / 注册表 / 标准安装位置现找。
        // 探测很快（几次 --version），启动时同步做，不像 svn 那样要进任务池
        let mut git_exe = PathBuf::from(&cfg.git_exe);
        if git_exe.as_os_str().is_empty() {
            if let Some((found, _)) = crate::git::detect(crate::git::candidates()) {
                git_exe = found;
            }
        }
        let git = crate::git::Git { exe: git_exe };
        let git_version = git.version();
        // 主窗口句柄只在启动时取一次：DWM 的原生标题栏配色只认 HWND，主题随时可切
        #[cfg(windows)]
        let hwnd = {
            use raw_window_handle::HasWindowHandle;
            cc.window_handle().ok().and_then(|handle| match handle.as_raw() {
                raw_window_handle::RawWindowHandle::Win32(win32) => {
                    Some(win32.hwnd.get() as isize)
                }
                _ => None,
            })
        };
        #[cfg(not(windows))]
        let hwnd = None;
        // 自绘标题栏：抹掉系统标题栏，天区 / 缩放 / 按钮命中全由界面层接管
        //（顶栏绘制在 header.rs，命中的原生半边在 titlebar.rs）
        #[cfg(windows)]
        if let Some(hwnd) = hwnd {
            crate::titlebar::install(hwnd);
        }
        // 通知身份：注册自己的 AUMID（显示名 + 图标），系统通知显示本程序而非借用的 PowerShell
        crate::notify::ensure_identity();
        // 右上角与窗口控制键的 SVG 图标：启动时栅格化一次，之后每帧只是贴图
        let icons = crate::icons::IconSet::load(&cc.egui_ctx);
        let mut app = Self {
            dirs: vec![DirView::default(); cfg.dirs.len()],
            probing: false,
            refresh_after_detect: true,
            hwnd,
            caption_dark: None,
            corner_maximized: None,
            version,
            candidates: Vec::new(),
            cfg,
            svn,
            git,
            git_version,
            bc,
            pool: Pool::default(),
            output: Vec::new(),
            page: Page::Main,
            commit: None,
            history: None,
            file_diff: None,
            file_log: None,
            selected: None,
            new_path: String::new(),
            new_label: String::new(),
            edit_label: None,
            label_buf: String::new(),
            label_focus: false,
            edit_group: None,
            group_buf: String::new(),
            group_focus: false,
            edit_tag: None,
            tag_buf: String::new(),
            tag_focus: false,
            drag: None,
            drag_pending: None,
            drag_pos: None,
            drag_target: None,
            drop_zones: Vec::new(),
            relocate: None,
            upload_all: None,
            batch_relocate: None,
            batch_label: None,
            rollback: None,
            resolve: None,
            merge: None,
            commit_rules: None,
            svn_user: String::new(),
            user_probed: false,
            show_settings: false,
            show_plugins: false,
            plugin_present: None,
            confirm_remove: None,
            auto_scroll: true,
            next_refresh: Instant::now() + Duration::from_secs(4),
            pending_refresh: Vec::new(),
            notify_queue: Vec::new(),
            font_note,
            update_info: None,
            update_ready: false,
            // 启动 3 秒后开始检查更新：避开启动瞬间的 svn 探测高峰
            update_check_at: Some(Instant::now() + Duration::from_secs(3)),
            // 开关打开后每隔 5 分钟查一次；这里先给一个周期，开启时会重置
            next_update_check: Instant::now() + Duration::from_secs(update::AUTO_CHECK_EVERY_SECS),
            show_update_confirm: false,
            update_error: None,
            ai_picks: Vec::new(),
            ai_pick_dir: 0,
            show_worklog: false,
            ai_mode: false,
            ai_extra: String::new(),
            ai_result: String::new(),
            ai_error: String::new(),
            mascot: mascot_texture(&cc.egui_ctx),
            logo: logo_texture(&cc.egui_ctx),
            icons: Some(icons),
        };
        app.push(Level::Info, format!("{APP_TITLE} 已启动（配置：{}）", config::config_file().display()));
        // 后端要在任何检测之前定下来：sync_views 之前每个 DirView 的 kind 还是默认的 Svn，
        // 先刷新一轮就会拿 svn 去问 Git 目录，报出「不是 SVN 工作副本 E155007」
        app.sync_views();
        // 自更新的残局在这里收：上次换名留下的 `.old.exe`（旧进程刚退出才删得掉）、
        // 以及下载完没走到换名的暂存文件
        if let Ok(exe) = std::env::current_exe() {
            if update::clean_leftovers(&exe) {
                app.hint(format!("已更新到 V{APP_VERSION}，旧程序文件已清理"));
                app.push(
                    Level::Success,
                    format!("本次是自更新后的第一次启动：V{APP_VERSION} 已就位"),
                );
            }
        }
        if app.svn.available() {
            app.push(
                Level::Success,
                format!(
                    "svn.exe = {}（版本 {}）",
                    app.svn.exe.display(),
                    app.version.clone().unwrap_or_default()
                ),
            );
            app.spawn_all_refresh();
            app.probing = false;
        } else {
            app.spawn_detect();
        }
        if app.git.available() {
            app.push(
                Level::Info,
                format!(
                    "git.exe = {}（版本 {}）",
                    app.git.exe.display(),
                    app.git_version.clone().unwrap_or_default()
                ),
            );
        }
        app
    }

    // ------------------------------------------------------------ 小工具

    pub fn push(&mut self, level: Level, text: impl Into<String>) {
        for line in text.into().lines() {
            if !line.trim().is_empty() {
                self.output.push(OutLine {
                    level,
                    text: line.to_owned(),
                });
            }
        }
        if self.output.len() > 3000 {
            let cut = self.output.len() - 3000;
            self.output.drain(..cut);
        }
    }

    /// 提示不再常驻在底部那一行，直接作为一条 INFO 写进上面的日志区
    pub fn hint(&mut self, text: impl Into<String>) {
        self.push(Level::Info, text);
    }

    /// 测试用：日志末尾几条拼起来看（一条提示可能占好几行，只看最后一条会截到半句）
    #[cfg(test)]
    pub(crate) fn log_tail(&self) -> String {
        let from = self.output.len().saturating_sub(4);
        self.output[from..]
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn persist(&mut self) {
        if let Err(e) = self.cfg.save() {
            self.hint(format!("保存配置失败：{e}"));
        }
    }

    pub fn dir_path(&self, index: usize) -> Option<PathBuf> {
        self.cfg.dirs.get(index).map(|d| PathBuf::from(&d.path))
    }

    pub fn dir_label(&self, index: usize) -> String {
        match self.cfg.dirs.get(index) {
            Some(dir) if !dir.label.trim().is_empty() => dir.label.trim().to_owned(),
            Some(dir) => Path::new(&dir.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.path.clone()),
            None => format!("#{index}"),
        }
    }

    pub(crate) fn sync_views(&mut self) {
        self.dirs.resize(self.cfg.dirs.len(), DirView::default());
        self.dirs.truncate(self.cfg.dirs.len());
        // 后端类型跟着磁盘走：目录里有 .git 就是 Git，有 .svn 就是 SVN
        for (index, dir) in self.cfg.dirs.iter().enumerate() {
            if let Some(view) = self.dirs.get_mut(index) {
                if let Some(kind) = crate::repo::detect(Path::new(&dir.path)) {
                    view.kind = kind;
                }
            }
        }
        if self.selected.is_some_and(|i| i >= self.cfg.dirs.len()) {
            self.selected = None;
        }
    }

    pub fn apply_svn_exe(&mut self, exe: &str) {
        self.cfg.svn_exe = exe.trim().trim_matches('"').to_owned();
        self.svn.exe = PathBuf::from(&self.cfg.svn_exe);
        self.version = self.svn.version();
        self.svn_user.clear();
        self.user_probed = false;
        self.persist();
    }

    pub fn apply_git_exe(&mut self, exe: &str) {
        self.cfg.git_exe = exe.trim().trim_matches('"').to_owned();
        self.git.exe = PathBuf::from(&self.cfg.git_exe);
        self.git_version = self.git.version();
        self.persist();
    }

    pub fn svn_or_none(&mut self) -> Option<Svn> {
        if self.svn.available() {
            Some(self.svn.clone())
        } else {
            self.hint("未找到可用的 svn.exe，请在「设置」中指定路径或重新自动寻找");
            self.show_settings = true;
            None
        }
    }

    /// 该目录的后端类型。还没检测过时按 SVN 算（列表里躺着的老目录绝大多数是 SVN）。
    pub(crate) fn dir_kind(&self, index: usize) -> crate::repo::RepoKind {
        self.dirs
            .get(index)
            .map(|view| view.kind)
            .unwrap_or(crate::repo::RepoKind::Svn)
    }

    pub(crate) fn git_or_none(&mut self) -> Option<crate::git::Git> {
        if self.git.available() {
            Some(self.git.clone())
        } else {
            self.hint("未找到可用的 git.exe，请在「设置 → git.exe 路径」中指定");
            self.show_settings = true;
            None
        }
    }

    // ------------------------------------------------------------ 任务回收

    fn drain(&mut self, ctx: &egui::Context) {
        let polled = self.pool.poll();
        let mut busy = !polled.lines.is_empty() || !polled.done.is_empty() || !polled.stranded.is_empty();
        // 后台线程没回传结果就没了（svn 线程 panic、起线程失败）：必须说出来，否则界面就是「点了没反应」
        for (kind, dir, label) in polled.stranded {
            self.push(Level::Error, format!("任务「{label}」异常结束（{kind:?}，目录 #{dir}）"));
            self.hint(format!("「{label}」异常结束，未取得结果，请重试"));
        }
        for line in polled.lines {
            let level = if line.starts_with('$') || line.starts_with('→') {
                Level::Command
            } else if line.contains("失败")
                || line.contains("Error")
                || line.contains("svn: E")
                || line.contains("fatal:")
                || line.contains("error:")
            {
                Level::Error
            } else if line.contains("Warning")
                || line.contains("警告")
                || line.contains("冲突")
                || line.contains("warning:")
            {
                Level::Warning
            } else if line.contains("完成") || line.starts_with('A') || line.starts_with('D') {
                Level::Success
            } else {
                Level::Info
            };
            self.push(level, line);
        }
        for (kind, _dir, data) in polled.done {
            match (kind, data) {
                (Kind::DetectSvn, Data::Svn { exe, version, candidates }) => {
                    self.probing = false;
                    self.candidates = candidates;
                    if exe.is_empty() {
                        self.hint("未能自动找到 svn.exe，请在「设置」中手动指定路径");
                        self.push(Level::Error, "未找到 svn.exe：请安装命令行版 Subversion（svn.exe），或在设置里手动指定");
                        self.show_settings = true;
                    } else {
                        self.push(Level::Success, format!("已找到 svn.exe：{exe}（版本 {version}）"));
                        self.apply_svn_exe(&exe);
                        self.hint(format!("已自动连接 svn.exe（版本 {version}）"));
                        if self.refresh_after_detect {
                            self.spawn_all_refresh();
                        }
                    }
                    self.refresh_after_detect = false;
                }
                (Kind::AuthUser, Data::User { user }) => {
                    self.svn_user = user.clone();
                    self.push(Level::Info, format!("本机 svn 登录人：{user}"));
                    // 启动时可能还没查到，查到后补一次「只看自己」的加载
                    let mut again = None;
                    if let Some(history) = self.history.as_mut() {
                        if history.author.is_empty() {
                            history.author = user;
                            history.mine = true;
                            again = Some(history.dir);
                        }
                    }
                    if let Some(dir) = again {
                        // 页面上已经按区间读的话，补读也要按同一条区间来，不能把用户筛出来的列表换成默认那批
                        let (unlimited, range) = self
                            .history
                            .as_ref()
                            .map(|page| (page.unlimited, page.range))
                            .unwrap_or((false, None));
                        self.spawn_log_in(dir, true, unlimited, range);
                    }
                }
                (Kind::CheckUpdate, Data::UpdateCheck { ok, message, info, ready, quiet }) => {
                    if !ok {
                        // 启动时的自动检查失败只温和提醒（服务器没开是常态），不弹错误打断使用；
                        // 定时那一轮直接闭嘴，下一轮到点再试
                        if !quiet {
                            self.push(Level::Warning, format!("检查更新失败：{message}"));
                        }
                    } else {
                        match info {
                            Some(manifest) => {
                                let latest = manifest.version.trim().to_owned();
                                let notes = manifest.notes.trim().to_owned();
                                self.update_info = Some(manifest);
                                self.update_ready = ready;
                                if ready {
                                    // 版本号没动、只是换了构建时不能报「新版本 V1.2.4（当前 V1.2.4）」
                                    let replaced = update::same_version(&latest, APP_VERSION);
                                    self.push(
                                        Level::Warning,
                                        if replaced {
                                            format!("发现新版本 V{latest}（新构建）")
                                        } else {
                                            format!("发现新版本 V{latest}（当前 V{APP_VERSION}）")
                                        },
                                    );
                                    if !notes.is_empty() {
                                        self.push(Level::Info, format!("更新说明：\n{notes}"));
                                    }
                                    self.hint(format!(
                                        "发现可更新版本 V{latest}，点击顶部「↑ 新版本」按钮即可更新"
                                    ));
                                } else if !quiet {
                                    // 定时检查没查到新版本就什么都不说，日志区只留 svn 操作
                                    self.push(
                                        Level::Success,
                                        format!("已是最新版本（V{APP_VERSION}）"),
                                    );
                                }
                            }
                            None => self.push(Level::Warning, "更新源没有返回版本信息"),
                        }
                    }
                }
                (Kind::DownloadUpdate, Data::UpdateDownloaded { ok, message, bytes }) => {
                    if !ok {
                        // 失败原因也放进更新对话框里内联显示，不止在日志区
                        self.update_error = Some(message.clone());
                        self.hint(format!("下载新版本失败：{message}"));
                        self.push(Level::Error, format!("下载新版本失败：{message}"));
                    } else {
                        // 下载与校验都已就绪：把新版换到正式名字下并重启（会退出本程序）
                        self.apply_downloaded_update();
                        let _ = bytes;
                    }
                }
                (Kind::AiLog, Data::AiLog { ok, content, message }) => {
                    // 结果整块回传（非流式）：成功直接放进生成窗口，失败原因也显示在窗口里
                    if ok {
                        self.ai_result = content.clone();
                        self.ai_error.clear();
                        self.push(
                            Level::Success,
                            format!("AI 日志已生成（{} 字），可在「AI 工作日志」窗口查看与复制", content.chars().count()),
                        );
                        self.hint("AI 日志已生成");
                    } else {
                        self.ai_error = message.clone();
                        self.push(Level::Error, format!("AI 生成日志失败：{message}"));
                    }
                }
                (
                    Kind::Refresh,
                    Data::Wc {
                        dir: index,
                        info,
                        remote,
                        remote_msg,
                        remote_rev,
                        last_rev,
                        last_author,
                        last_date,
                        changed,
                        changes,
                        all_changes,
                        conflicts,
                        blocked,
                        out_of_date,
                    },
                ) => {
                    if let Some(view) = self.dirs.get_mut(index) {
                        view.info = info;
                        view.remote = remote;
                        view.remote_msg = remote_msg;
                        view.remote_rev = remote_rev;
                        view.last_rev = last_rev;
                        view.last_author = last_author;
                        view.last_date = last_date;
                        view.changed = changed;
                        view.changes = changes;
                        view.all_changes = all_changes;
                        view.conflicts = conflicts;
                        view.blocked = blocked;
                        view.out_of_date = out_of_date;
                        view.checked_at = chrono::Local::now().format("%H:%M:%S").to_string();
                    }
                    // 更新通知订阅：远端可达、确实有未更新项、版本号没通知过才入队；
                    // 入队即记下该版本，同一帧内重复结果 / 下一轮检测都不会再报
                    let label = self.dir_label(index);
                    if let Some(view) = self.dirs.get(index) {
                        if let Some(dir_cfg) = self.cfg.dirs.get_mut(index) {
                            if dir_cfg.notify
                                && view.remote == Some(true)
                                && view.out_of_date.unwrap_or(0) > 0
                                && !view.remote_rev.is_empty()
                                && view.remote_rev != dir_cfg.notified_rev
                            {
                                dir_cfg.notified_rev = view.remote_rev.clone();
                                self.notify_queue.push(crate::notify::UpdateNotice {
                                    label,
                                    rev: view.remote_rev.clone(),
                                    author: view.last_author.clone(),
                                    behind: view.out_of_date.unwrap_or(0),
                                });
                            }
                        }
                    }
                    if let Some(quiet) = self
                        .pending_refresh
                        .iter()
                        .find(|(item, _)| *item == index)
                        .map(|&(_, quiet)| quiet)
                    {
                        self.pending_refresh.retain(|(item, _)| *item != index);
                        self.spawn_refresh(index, quiet);
                    }
                }
                (Kind::Diagnose, Data::Run { dir: _, ok, message, .. }) => {
                    // 报告已经写成文件了，这里只报结果，不用跟着刷新整棵树
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                }
                (Kind::LocalDiff, Data::LocalDiff { dir: index, ok, summary }) => {
                    if let Some(view) = self.dirs.get_mut(index) {
                        // 读失败同样清成 None：本地端比对慢几秒，拿上一次的数冒充现状最坏
                        view.local_diff = ok.then_some(summary);
                        view.local_checked_at = if ok {
                            chrono::Local::now().format("%H:%M:%S").to_string()
                        } else {
                            String::new()
                        };
                    }
                }
                (Kind::Merge, Data::MergePlan { dir, ok, plan }) => {
                    // 只往还开着的那扇窗口里放：预览是那一刻的状态，窗口关了就丢掉，
                    // 下次开窗口重算一份才是真的现状
                    if let Some(preview) = self.merge.as_mut().filter(|p| p.dir == dir) {
                        preview.plan = plan.map(|boxed| *boxed);
                        preview.error = if ok {
                            String::new()
                        } else {
                            "合并预览没算出来，原因见输出区".to_owned()
                        };
                    }
                }
                (Kind::ApplyMerge, Data::Run { dir, ok, message, reload }) => {
                    if ok {
                        // 落盘成了就关窗：这份清单已经应用完了，留着只会让人再点一次
                        self.merge = None;
                        self.hint(message.clone());
                    } else if let Some(preview) = self.merge.as_mut().filter(|p| p.dir == dir) {
                        preview.error = message.clone();
                        preview.confirm = false;
                    }
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    // 半途失败也照样重读：已经写出去的那几项得在差异数和「待提交」里如实反映
                    if reload {
                        self.spawn_status(dir, false);
                        self.spawn_refresh(dir, false);
                    }
                }
                (Kind::CleanBackup, Data::Run { dir, ok, message, .. }) => {
                    // 备份栏重扫一遍：删掉的几条得从清单里下去，确认也顺手收掉。
                    // 没动两边的文件，就不必跟着补检测
                    if let Some(preview) = self.merge.as_mut().filter(|p| p.dir == dir) {
                        preview.backups = crate::merge::backups(&crate::config::config_dir());
                        preview.wipe = false;
                    }
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                }
                (Kind::Status, Data::Status { dir: index, entries, ok, message }) => {
                    // 先数完再把 entries 交给提交页（它拿走所有权）
                    let blocked = ok.then(|| crate::svn::blocked_count(&entries));
                    let listed = ok.then(|| crate::svn::blocked_entries(&entries));
                    // 借用 view 之前先把这两样取出来：主页的数要和提交页用同一个口径
                    let root = self.dir_path(index);
                    let excludes = self
                        .cfg
                        .dirs
                        .get(index)
                        .map(|dir| dir.commit_excludes.clone())
                        .unwrap_or_default();
                    if let Some(view) = self.dirs.get_mut(index) {
                        view.conflicts = blocked;
                        view.blocked = listed.unwrap_or_default();
                        if ok {
                            // 口径与「全部上传」一致：? / ! 提交时会自动补 add / delete，一并算待提交
                            view.changes = root.as_ref().map_or_else(
                                || {
                                    entries
                                        .iter()
                                        .filter(|entry| entry.item.uploadable())
                                        .cloned()
                                        .collect()
                                },
                                |root| crate::commit::pending_entries(&entries, root, &excludes),
                            );
                            view.changed = Some(view.changes.len());
                            view.all_changes = entries.clone();
                        } else {
                            // 读失败就清掉旧数：宁可显示「未检测」也不能拿旧清单冒充现状
                            view.changed = None;
                            view.changes.clear();
                            view.all_changes.clear();
                        }
                    }
                    // 「解决冲突」窗口开着时这一趟就是它的最新依据：窗口里可能已经挑好策略，
                    // rebuild 按路径把挑的保住；条目全没了就关窗，别让人对着一张空表点执行
                    if ok {
                        let closed = match self.resolve.as_mut().filter(|panel| panel.dir == index) {
                            Some(panel) => {
                                panel.rebuild(&entries);
                                panel.rows.is_empty() && panel.incomplete == 0
                            }
                            None => false,
                        };
                        if closed {
                            self.resolve = None;
                            self.hint("这个目录的冲突已经没有了");
                        }
                    }
                    if let Some(commit) = self.commit.as_mut() {
                        if commit.dir == index {
                            if ok {
                                commit.set_entries(entries);
                            } else {
                                commit.error = message.clone();
                            }
                        }
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取修改状态失败：{message}"));
                    }
                }
                (Kind::Log, Data::Log { dir: index, entries, ok, message }) => {
                    if let Some(history) = self.history.as_mut() {
                        if history.dir == index {
                            if ok {
                                history.entries = entries;
                            } else {
                                history.error = message.clone();
                            }
                        }
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取提交记录失败：{message}"));
                    }
                }
                (Kind::Commit, Data::Run { dir, ok, message, reload }) => {
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    if let Some(commit) = self.commit.as_mut() {
                        commit.done_ok = ok;
                        // 先清空避免重复提交已提交的条目，再重新读 status 取回真正剩下的改动
                        if ok {
                            commit.set_entries(Vec::new());
                        }
                    }
                    // 提交前的 svn add / svn delete 已经改过工作副本，提交没成功也要重读列表。
                    // 这两步都是程序自己补的自动刷新：静音执行，不在输出区刷 `$ svn …` 日志
                    if reload && self.commit.as_ref().is_some_and(|c| c.dir == dir) {
                        self.spawn_status(dir, true);
                    }
                    self.spawn_refresh(dir, true);
                }
                (Kind::Update, Data::Run { dir, ok, message, .. }) => {
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    self.spawn_refresh(dir, false);
                    if self.commit.as_ref().is_some_and(|c| c.dir == dir) {
                        self.spawn_status(dir, false);
                    }
                    let again = self.history.as_mut().filter(|h| h.dir == dir).map(|h| {
                        h.entries.clear();
                        h.picked = None;
                        (h.mine, h.unlimited, h.range)
                    });
                    if let Some((mine, unlimited, range)) = again {
                        self.spawn_log_in(dir, mine, unlimited, range);
                    }
                }
                (Kind::Maintain, Data::Run { dir, ok, message, reload }) => {
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    if reload {
                        self.spawn_status(dir, false);
                        self.spawn_refresh(dir, false);
                    }
                }
                (Kind::Resolve, Data::Run { dir, ok, message, .. }) => {
                    // 成了不关窗：跟着重读一遍 status，还剩几条就是几条，全清了才在
                    // Status 那段把窗口收掉——一把梭最容易漏掉的就是「以为全成了」
                    self.hint(message.clone());
                    self.push(if ok { Level::Success } else { Level::Error }, message.clone());
                    if !ok {
                        if let Some(panel) = self.resolve.as_mut().filter(|p| p.dir == dir) {
                            panel.error = message;
                        }
                    }
                    self.spawn_status(dir, false);
                    self.spawn_refresh(dir, false);
                }
                (Kind::Rollback, Data::Run { dir, ok, message, reload }) => {
                    if ok {
                        // 回退成了就关窗，改动已经在「待提交」里等着核对
                        self.rollback = None;
                        self.hint(message.clone());
                    } else if let Some(dialog) = self.rollback.as_mut().filter(|d| d.dir == dir) {
                        dialog.error = message.clone();
                    }
                    self.push(if ok { Level::Success } else { Level::Error }, message);
                    // 失败也可能是 merge 做成一半，照样重读一遍才是真的状态
                    if reload {
                        self.spawn_status(dir, false);
                        self.spawn_refresh(dir, false);
                    }
                }
                (Kind::Relocate, Data::Run { dir, ok, message, .. }) => {
                    self.push(if ok { Level::Success } else { Level::Error }, message.clone());
                    if ok {
                        self.relocate = None;
                        self.hint(message);
                    } else if let Some(dialog) = self.relocate.as_mut().filter(|d| d.dir == dir) {
                        dialog.error = message;
                    }
                    self.spawn_refresh(dir, false);
                }
                (Kind::FileDiff, Data::Run { dir: index, ok, message, .. }) => {
                    if let Some(diff) = self.file_diff.as_mut().filter(|diff| diff.dir == index) {
                        diff.diff = message.clone();
                        diff.error = if ok { String::new() } else { message.clone() };
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取该次提交的差异失败：{message}"));
                    }
                }
                (Kind::FileLog, Data::Log { dir: index, entries, ok, message }) => {
                    if let Some(log) = self.file_log.as_mut().filter(|log| log.dir == index) {
                        log.entries = entries;
                        log.error = if ok { String::new() } else { message.clone() };
                    }
                    if !ok {
                        self.push(Level::Error, format!("读取文件提交记录失败：{message}"));
                    }
                }
                (Kind::Diff, Data::Run { dir, ok, mut message, .. }) => {
                    if !ok && message.trim().is_empty() {
                        message = "无法取得差异（svn diff 执行失败）".to_owned();
                    }
                    // 忽略空白后为空，说明这一项只改了缩进 / 空格 / 换行符，不写清楚会以为读挂了
                    if ok && message.trim().is_empty() {
                        let ignored = self.commit.as_ref().is_some_and(|commit| commit.diff_ignore);
                        message = if ignored {
                            "忽略空白与换行后没有内容改动：这一项只是缩进、空格或换行符（CRLF↔LF）变了。\n取消勾选「忽略空白与换行」就能看到整份文件被逐行替换。".to_owned()
                        } else {
                            "没有差异：该文件内容与 BASE 版本一致。".to_owned()
                        };
                    }
                    if let Some(commit) = self.commit.as_mut().filter(|commit| commit.dir == dir) {
                        commit.diff = message;
                    }
                }
                (_, other) => {
                    if let Data::Run { dir, ok, message, .. } = other {
                        self.hint(message.clone());
                        self.push(if ok { Level::Success } else { Level::Error }, message);
                        self.spawn_refresh(dir, false);
                    }
                }
            }
            busy = true;
        }
        // 本帧收集齐的更新订阅统一发一条系统通知：多个目录合并为一条，不逐目录连环弹。
        // 已通知版本号在入队时就写进了配置，这里无论发送成功与否都持久化，避免失败后反复重试弹窗
        if !self.notify_queue.is_empty() {
            let items: Vec<_> = self.notify_queue.drain(..).collect();
            self.persist();
            match crate::notify::send_update_toast(&items) {
                Ok(()) => {
                    let names = items
                        .iter()
                        .map(|item| item.label.as_str())
                        .collect::<Vec<_>>()
                        .join("、");
                    self.push(Level::Info, format!("已发送更新通知：{names}"));
                }
                Err(e) => self.push(Level::Warning, format!("发送系统通知失败：{e}")),
            }
        }
        if busy {
            ctx.request_repaint();
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(Duration::from_millis(150));
        // 主题随时可切换（顶部按钮 / 设置里选），这里统一同步给 egui；
        // 原生标题栏不再交给 sync_window_theme 的出厂色，下面按当前视觉自己写给 DWM
        let preference = match self.cfg.theme.as_str() {
            "light" => egui::ThemePreference::Light,
            "dark" => egui::ThemePreference::Dark,
            _ => egui::ThemePreference::System,
        };
        if ctx.options(|opt| opt.theme_preference) != preference {
            ctx.set_theme(preference);
        }
        // 首帧设一次，之后只有深浅真的换了才重设（DWM 调用不便宜，也没必要逐帧发）
        let dark = ctx.theme() == egui::Theme::Dark;
        if self.caption_dark != Some(dark) {
            self.caption_dark = Some(dark);
            let style = ctx.style_of(ctx.theme());
            style_title_bar(
                self.hwnd,
                dark,
                style.visuals.panel_fill,
                style.visuals.widgets.noninteractive.fg_stroke.color,
            );
        }
        // 圆角随最大化切换：非最大化四角圆角（Win11），最大化直角；状态变了才发一次
        let maximized = ctx.input(|i| i.viewport().maximized) == Some(true);
        if self.corner_maximized != Some(maximized) {
            self.corner_maximized = Some(maximized);
            if let Some(hwnd) = self.hwnd {
                crate::titlebar::refresh_shape(hwnd, maximized);
            }
        }
        self.drain(ctx);
        self.spawn_auth_user();
        // 启动后到点的第一次版本更新检查：官方源不用填地址就能查；
        // 自定义源要填了地址才查，地址为空就安静跳过（不催用户配置）
        if let Some(at) = self.update_check_at {
            if Instant::now() >= at {
                self.update_check_at = None;
                if self.cfg.check_update_on_start && self.update_source_ready() {
                    self.spawn_update_check(false);
                }
            }
        }
        // 自动检查更新：开关打开后每 5 分钟查一次。正在查的时候不重复起，
        // 过程静音（只有发现新版本或失败才在日志区说一声）
        if self.cfg.auto_update_check
            && Instant::now() >= self.next_update_check
            && self.update_source_ready()
            && !self.pool.has(Kind::CheckUpdate, usize::MAX)
        {
            self.next_update_check =
                Instant::now() + Duration::from_secs(update::AUTO_CHECK_EVERY_SECS);
            self.spawn_update_check(true);
        }
        if self.cfg.auto_refresh > 0
            && !self.probing
            && self.pool.running() == 0
            && Instant::now() >= self.next_refresh
            && !self.cfg.dirs.is_empty()
        {
            self.next_refresh = Instant::now() + Duration::from_secs(self.cfg.auto_refresh);
            self.spawn_all_refresh();
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            if path.is_dir() {
                self.add_directory(path.display().to_string());
            }
        }
    }

    // ------------------------------------------------------------ 底部输出面板

    /// 收起时面板的总高（含面板 frame 上下各 2 的边距）：顶行 28 + 4
    const LOG_COLLAPSED_HEIGHT: f32 = 32.0;

    /// 日志面板当前是否收起：用户手动切过就按手动来，没动过则跟随主页样式（简约收起、详细展开）
    pub fn log_collapsed(&self) -> bool {
        self.cfg.log_collapsed.unwrap_or(self.cfg.home_compact)
    }

    /// 切换收起状态并落盘；没变化就不写配置
    fn set_log_collapsed(&mut self, collapsed: bool) {
        if self.cfg.log_collapsed != Some(collapsed) {
            self.cfg.log_collapsed = Some(collapsed);
            self.persist();
        }
    }

    /// 组建底部日志面板：收起时锁死一行高并关掉拖拽，展开时恢复默认高度。
    /// 收起那阵子 exact_size 会把面板高度写进状态存成一行高，直接放开会沿用那个
    /// 矮尺寸，展开前量一眼上次的实际高度，太矮就抬回默认
    fn output_panel_builder(&self, ui: &Ui) -> egui::Panel {
        let collapsed = self.log_collapsed();
        let mut panel = egui::Panel::bottom("output");
        if collapsed {
            panel = panel.exact_size(Self::LOG_COLLAPSED_HEIGHT).resizable(false);
        } else {
            let last = egui::PanelState::load(ui.ctx(), egui::Id::new("output"))
                .map(|state| state.size().y)
                .unwrap_or(190.0);
            panel = if last < Self::LOG_COLLAPSED_HEIGHT + 8.0 {
                panel.exact_size(190.0)
            } else {
                panel.default_size(190.0)
            };
            panel = panel.resizable(true);
        }
        panel
    }

    fn output_panel(&mut self, ui: &mut Ui) {
        let count = self.output.len();
        // 连接状态从顶栏搬到这里：与标题、操作键同一行，占中间位置
        let (color, conn_text, conn_reason) = self.connection();
        let color = ink(ui, color);
        let running = self.pool.running();
        let busy_label = self
            .selected
            .and_then(|index| self.pool.busy_label(index))
            .or_else(|| if running > 0 { Some("任务执行中".into()) } else { None });
        // 顶行三块：左 = 标题 + 清空，中 = 连接状态（含任务占用提示），右 = 配置入口与自动滚动；
        // 行高取控件自然高略放一档（28），三块各自在行内垂直居中
        let full = ui.available_rect_before_wrap();
        let row = egui::Rect::from_min_size(full.min, egui::Vec2::new(full.width(), 28.0));
        ui.allocate_rect(row, egui::Sense::hover());
        let mut left = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("output_bar_left")
                .max_rect(row)
                .layout(Layout::left_to_right(Align::Center)),
        );
        left.label(RichText::new(format!("输出运行（{count} 行）")).strong().size(13.0));
        if quiet(&mut left, |ui| ui.button("清空")).clicked() {
            self.output.clear();
        }
        let left_end = left.min_rect().max.x;
        drop(left);
        // 中段：嵌套 horizontal 会把水平对齐重置回贴左，直接套居中布局套不住它。
        // 先量出「转圈 + 状态文字 + 任务提示」的自然总宽，再以行中心为锚、
        // 用左对齐布局从算好的起点画，两头的宽度不够时各自留 8pt 间隙让位
        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("output_bar_right")
                .max_rect(row)
                .layout(Layout::right_to_left(Align::Center)),
        );
        // 收起 / 展开键放在最右：right_to_left 布局先画的占最右位
        let collapsed = self.log_collapsed();
        let toggle_label = if collapsed { "展开" } else { "收起" };
        let mut toggle_to = None;
        if quiet(&mut right, |ui| ui.button(toggle_label)).clicked() {
            toggle_to = Some(!collapsed);
        }
        let mut auto_scroll = self.auto_scroll;
        right.checkbox(&mut auto_scroll, "自动滚动");
        self.auto_scroll = auto_scroll;
        if quiet(&mut right, |ui| ui.button("打开配置目录")).clicked() {
            let dir = config::config_file()
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let _ = std::process::Command::new("explorer").arg(dir).spawn();
        }
        if let Some(to) = toggle_to {
            self.set_log_collapsed(to);
        }
        let right_start = right.min_rect().min.x;
        drop(right);
        let gap = ui.spacing().item_spacing.x;
        let body_size = ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Body)
            .map(|f| f.size)
            .unwrap_or(13.0);
        let conn = format!("● {conn_text}");
        let conn_w = ui
            .painter()
            .layout_no_wrap(conn.clone(), egui::FontId::proportional(body_size), color)
            .size()
            .x;
        let busy_text = busy_label
            .as_ref()
            .map(|label| format!("{running} 个任务：{label}"));
        let busy_w = busy_text.as_ref().map_or(0.0, |text| {
            ui.painter()
                .layout_no_wrap(text.clone(), egui::FontId::proportional(12.0), egui::Color32::WHITE)
                .size()
                .x
        });
        let spinner_w = if running > 0 { Some(14.0) } else { None };
        let mut total = conn_w + busy_w;
        if busy_text.is_some() {
            total += gap;
        }
        if spinner_w.is_some() {
            total += 14.0 + gap;
        }
        let mid_left = (row.center().x - total / 2.0)
            .max(left_end + gap)
            .min((right_start - gap - total).max(left_end + gap));
        let mut mid = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("output_bar_conn")
                .max_rect(egui::Rect::from_min_size(
                    egui::pos2(mid_left, row.min.y),
                    egui::Vec2::new(row.right() - mid_left, row.height()),
                ))
                .layout(Layout::left_to_right(Align::Center)),
        );
        if running > 0 {
            mid.add(egui::Spinner::new().size(14.0));
        }
        mid.label(RichText::new(&conn).color(color).strong()).on_hover_text(conn_reason);
        if let Some(text) = &busy_text {
            mid.label(RichText::new(text).weak().size(12.0));
        }
        drop(mid);
        // 收起状态下面板只剩顶行：分隔线和日志主体都不画，行高刚好撑满面板
        if collapsed {
            return;
        }
        // 子 Ui 不推父级光标，这里自己把顶行的高度吃掉。
        // 分隔线前的 item_spacing.y（8px）要清掉：不然空隙全落在行的下方，
        // 行内容虽然居中，看起来却整体偏上（下方留白明显大于上方）
        // 子 Ui 不推父级光标，这里自己把顶行的高度吃掉。
        // advance 和分隔线都会各自加一道 item_spacing.y，空隙全落在行的下方——
        // 行内容虽然居中，看起来却整体偏上（下方留白明显大于上方）。
        // 所以先清零再 advance，分隔线也收窄到 1px 贴住行底，画完恢复原间距
        let row_gap = ui.spacing_mut().item_spacing.y;
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.advance_cursor_after_rect(row);
        ui.add(egui::Separator::default().spacing(1.0));
        ui.spacing_mut().item_spacing.y = row_gap;
        let auto_scroll = self.auto_scroll;
        let output = self.output.clone();
        ScrollArea::vertical()
            .id_salt("output_area")
            .auto_shrink([false, false])
            .stick_to_bottom(auto_scroll)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for line in output.iter().rev().take(600).rev() {
                    let color = if line.level == Level::Info {
                        ui.visuals().weak_text_color()
                    } else {
                        line.level.color(ui)
                    };
                    ui.monospace(RichText::new(&line.text).size(12.0).color(color));
                }
            });
    }
}

/// 原生标题栏跟着当前视觉走。eframe 自带的 sync_window_theme 只会切「出厂深 / 出厂浅」，
/// 切不出我们这套冷白与冷黑，顶部于是总像别人家的窗口；Win11 的 DWM 允许直接把
/// 标题栏底色和文字色写掉，于是那一条终于和界面是同一套颜色。
/// 非 Windows、拿不到 HWND、或者系统不认这些属性（Win10 及更早）——一律什么都不做，
/// 出厂标题栏照样能用，不值得为配色报错。
fn style_title_bar(hwnd: Option<isize>, dark: bool, fill: egui::Color32, text: egui::Color32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
            DwmSetWindowAttribute,
        };
        let Some(hwnd) = hwnd else { return };
        let hwnd = hwnd as *mut core::ffi::c_void;
        // COLORREF 的字节序是 0x00BBGGRR，和 Color32 的 RGBA 正好反着
        let colorref = |color: egui::Color32| {
            let [r, g, b, _] = color.to_array();
            ((b as u32) << 16) | ((g as u32) << 8) | r as u32
        };
        let immersive: i32 = if dark { 1 } else { 0 };
        let caption = colorref(fill);
        let foreground = colorref(text);
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &immersive as *const i32 as *const _,
                4,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_CAPTION_COLOR as u32,
                &caption as *const u32 as *const _,
                4,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_TEXT_COLOR as u32,
                &foreground as *const u32 as *const _,
                4,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = (hwnd, dark, fill, text);
}

/// 右下角吉祥物的显示高度（逻辑点）：原图 872×1160，宽度按原比例跟着算。
/// 日志面板被拖矮时按面板高收，不然头顶会被面板裁掉
const MASCOT_H: f32 = 200.0;

/// 那张贴图的不透明度 0.2（× 255 取整）
const MASCOT_ALPHA: u8 = 51;

/// 启动时把嵌进 exe 的 PNG 解成贴图；解不出来就返回 None，界面少一枚贴图而已
fn mascot_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let rgba = image::load_from_memory(include_bytes!("../mascot.png"))
        .ok()?
        .to_rgba8();
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    );
    Some(ctx.load_texture("mascot", image, egui::TextureOptions::LINEAR))
}

/// 顶栏标题左边的软件标志：与窗口图标同一来源（rabbit.png）
fn logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let rgba = image::load_from_memory(include_bytes!("../rabbit.png"))
        .ok()?
        .to_rgba8();
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    );
    Some(ctx.load_texture("app-logo", image, egui::TextureOptions::LINEAR))
}

/// 贴在日志面板的右下角。只画不注册感应区，所以既不吃点击也不抢日志行的选中
fn paint_mascot(ui: &Ui, tex: &egui::TextureHandle) {
    let outer = ui.max_rect();
    let height = MASCOT_H.min(outer.height());
    let width = height * tex.aspect_ratio();
    ui.painter().image(
        tex.id(),
        egui::Rect::from_min_size(
            egui::pos2(outer.max.x - width, outer.max.y - height),
            egui::vec2(width, height),
        ),
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::from_white_alpha(MASCOT_ALPHA),
    );
}

impl eframe::App for RepoApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 字号 = 整屏缩放：egui 的 zoom_factor 是「系统 DPI × 这个倍数」，文字和控件一起放大。
        // 每帧对一次，设置页改完下一帧就生效，换显示器 / 改系统缩放也能跟上
        let want = fonts::scale_factor(&self.cfg.ui_scale);
        if ctx.zoom_factor() != want {
            ctx.set_zoom_factor(want);
        }
        self.tick(ctx);
    }

    /// eframe 默认的清屏色是近黑（12,12,12,180），面板之间的接缝只要有一像素没被底色盖住，
    /// 淡色描边就是往这支黑上混合——量出来 #E4E8F0 会变成 #313337，看着就是一条黑线。
    /// 底色跟着主题走，接缝处补上的就是同一档白
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // 顶栏就是自绘标题栏：左右留出一点呼吸位，高度与分隔线在 header() 里画
        egui::Panel::top("header")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 0)))
            .show(ui, |ui| {
                self.header(ui);
            });
        // 添加目录这条要顶到窗口两边，所以挂在中央面板之外（主页才画）
        if self.page == Page::Main {
            let (fill, line) = (pane_fill(ui), pane_stroke(ui));
            egui::Panel::top("add_dir_bar")
                .frame(
                    egui::Frame::new()
                        .fill(fill)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .stroke(egui::Stroke::new(1.0, line)),
                )
                .show(ui, |ui| self.add_dir_bar(ui));
        }
        // 提交页自动收起底部日志面板：改动列表要占满整窗高度，回主页 / 历史页自动恢复
        if self.page != Page::Commit {
            let collapsed = self.log_collapsed();
            self.output_panel_builder(ui).show(ui, |ui| {
                self.output_panel(ui);
                // 贴图压在日志之上，右下角贴边（0.2 不透明度，字照样读得清）；收起时不画
                if !collapsed {
                    if let Some(tex) = self.mascot.as_ref() {
                        paint_mascot(ui, tex);
                    }
                }
            });
        }
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            let page = self.page;
            match page {
                Page::Main => self.main_page(ui),
                Page::Commit => self.commit_page(ui),
                Page::History => self.history_page(ui),
            }
        });
        self.settings(&ctx);
        self.plugins_window(&ctx);
        self.update_confirm_dialog(&ctx);
        self.relocate_dialog(&ctx);
        self.batch_relocate_dialog(&ctx);
        self.batch_label_dialog(&ctx);
        self.rollback_dialog(&ctx);
        self.resolve_dialog(&ctx);
        self.merge_dialog(&ctx);
        self.upload_all_dialog(&ctx);
        self.commit_rules_dialog(&ctx);
        self.file_diff_window(&ctx);
        self.file_log_window(&ctx);
        self.worklog_window(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testbed::{base_input, stub_app};
    use egui::{Context, ThemePreference};

    /// 工具栏内容要在 40 高的标题栏里垂直居中：内容贴顶时底下空出一大截，
    /// 看着就像工具栏和添加目录栏之间隔了一条缝（2026-10-14 用户点名）；
    /// 添加栏的顶边也要贴着标题栏的发丝线，中间不再叠别的空隙
    #[test]
    fn header_content_is_centered_and_the_add_bar_hugs_it() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = stub_app(false);
        let mut input = base_input();
        let mut found = Vec::new();
        for _ in 0..2 {
            let mut out = ctx.run_ui(input, |ui| {
                egui::Panel::top("header")
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 0)))
                    .show(ui, |ui| app.header(ui));
                let (fill, line) = (crate::ui::pane_fill(ui), crate::ui::pane_stroke(ui));
                egui::Panel::top("add_dir_bar")
                    .frame(
                        egui::Frame::new()
                            .fill(fill)
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .stroke(egui::Stroke::new(1.0, line)),
                    )
                    .show(ui, |ui| app.add_dir_bar(ui));
            });
            input = base_input();
            out.textures_delta.clear();
            found = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(text) => {
                        let string = text.galley.text();
                        if string.contains(crate::APP_TITLE) {
                            Some(("title", text.pos.y + text.galley.size().y / 2.0))
                        } else if string.contains("添加仓库目录") {
                            Some(("add", text.pos.y))
                        } else {
                            None
                        }
                    }
                    egui::Shape::LineSegment { points, .. } => {
                        let (a, b) = (points[0], points[1]);
                        if (b.y - a.y).abs() < 0.5 && b.x - a.x > 800.0 {
                            Some(("hairline", a.y))
                        } else {
                            None
                        }
                    }
                    egui::Shape::Rect(rect) => {
                        (rect.stroke.width > 0.0 && rect.rect.width() > 800.0)
                            .then(|| ("bar_border", rect.rect.top()))
                    }
                    _ => None,
                })
                .collect();
        }
        let pick = |kind: &str| {
            found
                .iter()
                .find(|(k, _)| *k == kind)
                .map(|(_, v)| *v)
                .unwrap_or_else(|| panic!("一帧里找不到 {kind}：{found:?}"))
        };
        // 标题（工具栏里最高的那件内容之一）的中心要落在标题栏中线附近
        let title_center = pick("title");
        assert!(
            (title_center - 20.0).abs() <= 2.0,
            "工具栏内容要垂直居中（40 高的栏，中线 20），现在中心在 {title_center:.1}"
        );
        // 添加栏的顶边贴着发丝线：中间不许再有空隙
        let gap = pick("bar_border") - pick("hairline");
        assert!(
            (-1.0..=1.5).contains(&gap),
            "添加栏顶边要贴着标题栏的发丝线，现在隔了 {gap:.1}"
        );
        let _ = pick("add");
    }

    /// 嵌进 exe 的右下角贴图：解不出来只是少一枚贴图，跑起来才看得见，所以在测试里先解一次。
    /// 只验「解得开、不是全透明」，不去比具体像素
    #[test]
    fn embedded_mascot_png_decodes_to_something_visible() {
        let rgba = image::load_from_memory(include_bytes!("../mascot.png"))
            .expect("mascot.png 解不开")
            .to_rgba8();
        assert!(
            rgba.pixels().any(|p| p.0[3] > 0),
            "贴图整张都是透明的，画出来等于没画"
        );
        assert!(
            rgba.width() < rgba.height(),
            "贴图是竖构图才按高度铺得好看：实际 {}×{}",
            rgba.width(),
            rgba.height()
        );
    }

    /// 连接状态搬到日志面板顶行中间后，不许再和左区的「输出运行（N 行）/清空」叠在一起
    #[test]
    fn connection_status_in_the_middle_does_not_overlap_the_left_zone() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = stub_app(false);
        // 让顶行带上任务提示：挂一个忙碌任务 + 输出若干行
        app.hint("制造一些日志行");
        let mut input = base_input();
        let mut texts: Vec<(String, egui::Rect)> = Vec::new();
        // 两帧：第一帧布局，第二帧确认稳定
        for _ in 0..2 {
            let mut out = ctx.run_ui(input, |ui| app.output_panel(ui));
            input = base_input();
            out.textures_delta.clear();
            texts = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(text) => Some((
                        text.galley.text().to_string(),
                        item.shape.visual_bounding_rect(),
                    )),
                    _ => None,
                })
                .collect();
        }
        let find = |needle: &str| {
            texts
                .iter()
                .find(|(text, _)| text.contains(needle))
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("顶行里找不到「{needle}」：{texts:?}"))
        };
        let title = find("输出运行");
        let conn = find("●");
        // 与左区在水平方向上没有交叠
        assert!(
            conn.min.x > title.max.x + 4.0,
            "连接状态（{conn:?}）和左区（{title:?}）水平重叠了"
        );
        // 中段真的落在行的中部（测试窗口 1200 宽，中心应落在 25%~75% 之间）
        assert!(
            conn.center().x > 300.0 && conn.center().x < 900.0,
            "连接状态要落在行的中部，跑偏了：{conn:?}"
        );
    }

    /// 顶行与分隔线之间不能有 item_spacing 空隙：空隙全落在行下方时整行看起来偏上
    #[test]
    fn output_row_separator_hugs_the_row() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = stub_app(false);
        app.hint("制造一些日志行");
        let mut input = base_input();
        let mut lines: Vec<f32> = Vec::new();
        let mut texts: Vec<(String, egui::Rect)> = Vec::new();
        for _ in 0..2 {
            let mut out = ctx.run_ui(input, |ui| app.output_panel(ui));
            input = base_input();
            out.textures_delta.clear();
            lines = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::LineSegment { points, .. } => {
                        let (a, b) = (points[0], points[1]);
                        // 横跨大半窗口的水平线才是分隔线
                        ((a.y - b.y).abs() <= 0.5 && (a.x - b.x).abs() > 500.0)
                            .then(|| (a.y + b.y) / 2.0)
                    }
                    _ => None,
                })
                .collect();
            texts = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(text) => Some((
                        text.galley.text().to_string(),
                        item.shape.visual_bounding_rect(),
                    )),
                    _ => None,
                })
                .collect();
        }
        // 顶行高 28：分隔线必须贴着行底（y≈28），掉到 30 开外就是空隙没清掉
        let sep_y = lines.first().copied().unwrap_or_else(|| {
            panic!("顶行下没画横线：{lines:?}")
        });
        assert!(
            (sep_y - 28.0).abs() <= 2.0,
            "分隔线要贴着顶行底（y≈28），现在落在 y={sep_y:.1}，行下方留了空隙"
        );
        // 行内容（连接状态）在行内垂直居中
        let conn = texts
            .iter()
            .find(|(text, _)| text.contains('●'))
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("顶行里找不到连接状态：{texts:?}"));
        let top_gap = conn.min.y;
        let bottom_gap = 28.0 - conn.max.y;
        assert!(
            (top_gap - bottom_gap).abs() <= 1.5,
            "连接状态要垂直居中：上留白 {top_gap:.1} / 下留白 {bottom_gap:.1}"
        );
    }

    /// 提示不再占底部一行：hint() 的话要作为一条 INFO 落进日志，日志面板里只画一次
    #[test]
    fn hint_lands_in_the_log() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = stub_app(false);
        app.hint("已添加目录：C:\\Test");

        let last = app.output.last().expect("提示要进日志");
        assert_eq!(last.text, "已添加目录：C:\\Test");
        assert!(matches!(last.level, Level::Info), "提示按 INFO 记，不抢命令输出的颜色");

        let mut out = ctx.run_ui(base_input(), |ui| app.output_panel(ui));
        out.textures_delta.clear();
        let hits = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .filter(|text| text.contains("已添加目录"))
            .count();
        assert_eq!(hits, 1, "这条提示只该在日志里出现一次：{hits}");
    }

    /// 两个 GitHub 源（最新发布 / 测试分支）不用填任何地址；自建服务端源没地址就不算可用
    #[test]
    fn only_a_configured_source_is_ready() {
        let mut app = stub_app(false);
        app.cfg.update_source = update::SOURCE_OFFICIAL.to_owned();
        app.cfg.update_server.clear();
        assert!(app.update_source_ready(), "官方源总是可用");

        app.cfg.update_source = update::SOURCE_TEST.to_owned();
        assert!(app.update_source_ready(), "测试分支也是 GitHub 源，不用填地址");

        app.cfg.update_source = update::SOURCE_CUSTOM.to_owned();
        assert!(!app.update_source_ready(), "自定义源没填地址不该去查一个空地址");
        app.cfg.update_server = "http://192.168.1.251:20700".to_owned();
        assert!(app.update_source_ready());
    }

    /// 定时那一轮：没到点 / 没开开关都不该起任务；到点了起一次并把节拍往后推
    #[test]
    fn auto_check_fires_only_on_its_own_beat() {
        // 定时检查会写配置（spawn 里顺带 persist），别碰到用户真实的 config.json
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_auto_check_test"),
        );
        let ctx = Context::default();
        let mut app = stub_app(false);
        app.cfg.update_source = update::SOURCE_CUSTOM.to_owned();
        // 指向本机一个必定连不通的端口：这一轮只会失败，不会真去联网
        app.cfg.update_server = "http://127.0.0.1:9".to_owned();
        app.next_update_check = Instant::now() - Duration::from_secs(1);

        // 开关关着：到点也不动
        let before = app.next_update_check;
        app.cfg.auto_update_check = false;
        app.tick(&ctx);
        assert!(
            !app.pool.has(Kind::CheckUpdate, usize::MAX),
            "没开开关不该自动检查"
        );
        assert_eq!(app.next_update_check, before, "没开开关不该动节拍");

        // 开关打开：到点起一轮，并把下一次推到 5 分钟后
        app.cfg.auto_update_check = true;
        app.tick(&ctx);
        assert!(app.pool.has(Kind::CheckUpdate, usize::MAX), "到点该起一轮检查");
        assert!(
            app.next_update_check > Instant::now() + Duration::from_secs(update::AUTO_CHECK_EVERY_SECS - 2),
            "节拍要往后推一个周期：还有 {:.0}s",
            app.next_update_check
                .checked_duration_since(Instant::now())
                .map_or(0.0, |d| d.as_secs_f64()),
        );
    }

    /// 日志面板收起 / 展开：点顶行最右的「收起」后整条只剩一行（没有分隔线、没有日志
    /// 主体，面板高度锁在 32），状态写进配置下次启动生效；再点「展开」回到原样
    #[test]
    fn log_panel_collapse_toggles_and_persists() {
        // 切换会 persist 写 config.json，别碰用户真实配置
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_log_collapse_test"),
        );
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = stub_app(false);
        app.hint("制造一些日志行");
        assert!(!app.log_collapsed(), "详细样式下没动过就该展开");

        // 跑一帧完整面板（构建 + 顶行 + 主体），返回这一帧画出的形状
        let frame = |app: &mut RepoApp, click: Option<egui::Pos2>| {
            let mut input = base_input();
            if let Some(pos) = click {
                let press = |pressed: bool| egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                input.events = vec![egui::Event::PointerMoved(pos), press(true), press(false)];
            }
            let mut out = ctx.run_ui(input, |ui| {
                app.output_panel_builder(ui).show(ui, |ui| app.output_panel(ui));
            });
            out.textures_delta.clear();
            out
        };
        let text_rects = |out: &egui::FullOutput| {
            out.shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(text) => Some((
                        text.galley.text().to_string(),
                        item.shape.visual_bounding_rect(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        // 第一帧布局，找出「收起」键的位置
        let out = frame(&mut app, None);
        let toggle = text_rects(&out)
            .into_iter()
            .find(|(text, _)| text == "收起")
            .map(|(_, rect)| rect.center())
            .expect("展开状态下顶行最右应有「收起」键");

        // 点下去：状态落配置
        let _ = frame(&mut app, Some(toggle));
        assert_eq!(app.cfg.log_collapsed, Some(true), "收起状态要写进配置");
        assert!(app.log_collapsed());

        // 收起后的画面：键变「展开」，连接状态还在顶行，分隔线和日志主体都不画
        let out = frame(&mut app, None);
        let texts = text_rects(&out);
        assert!(texts.iter().any(|(t, _)| t == "展开"), "收起后键要变成「展开」");
        assert!(texts.iter().any(|(t, _)| t.contains('●')), "连接状态要留在顶行");
        assert!(
            !texts.iter().any(|(t, _)| t.contains("制造一些日志行")),
            "收起后日志主体不该画出来"
        );
        // 顶行下面再没有别的内容：分隔横线只许有一道（面板自己的上边缘线），
        // 展开时顶行下还有一道顶行分隔线 + 日志文本
        let sep_lines = out
            .shapes
            .iter()
            .filter(|item| match &item.shape {
                egui::Shape::LineSegment { points, .. } => {
                    let (a, b) = (points[0], points[1]);
                    (a.y - b.y).abs() <= 0.5 && (a.x - b.x).abs() > 500.0
                }
                _ => false,
            })
            .count();
        assert!(sep_lines <= 1, "收起后顶行下的分隔线不该再画：{sep_lines} 道");

        // 面板高度锁在顶行一行：32 = 行 28 + frame 上下各 2
        let height = egui::PanelState::load(&ctx, egui::Id::new("output"))
            .map(|state| state.size().y)
            .unwrap_or_default();
        assert!(
            (height - RepoApp::LOG_COLLAPSED_HEIGHT).abs() <= 1.0,
            "收起面板高度应为一行（{}），实际 {height:.1}",
            RepoApp::LOG_COLLAPSED_HEIGHT
        );

        // 再点「展开」：状态同样落配置
        let toggle = text_rects(&frame(&mut app, None))
            .into_iter()
            .find(|(text, _)| text == "展开")
            .map(|(_, rect)| rect.center())
            .expect("收起状态下顶行最右应有「展开」键");
        let _ = frame(&mut app, Some(toggle));
        assert_eq!(app.cfg.log_collapsed, Some(false), "展开状态也要落配置");
    }

    /// 简约样式默认收起日志面板：没手动动过（None）时跟着主页样式走；手动切过（Some）
    /// 就以手动为准，换样式也不变。老配置没有这个字段，读出来是 None，向后兼容
    #[test]
    fn compact_home_defaults_to_collapsed_log() {
        let app = stub_app(true);
        assert!(app.log_collapsed(), "简约样式没动过就该收起");
        let mut app = stub_app(false);
        assert!(!app.log_collapsed(), "详细样式没动过就该展开");

        app.cfg.log_collapsed = Some(true);
        assert!(app.log_collapsed(), "手动收起后换了样式也保持收起");
        app.cfg.home_compact = true;
        app.cfg.log_collapsed = Some(false);
        assert!(!app.log_collapsed(), "手动展开后换了样式也保持展开");

        let parsed: config::Config = serde_json::from_str(r#"{"home_compact":true}"#).unwrap();
        assert_eq!(parsed.log_collapsed, None, "老配置缺字段应读成 None");
        assert!(parsed.log_collapsed.unwrap_or(parsed.home_compact));
    }
}
