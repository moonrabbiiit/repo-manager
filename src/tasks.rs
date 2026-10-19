//! 每个 svn 动作的发起点：起后台任务（检测 / 更新 / 提交 / 日志 / 维护 / relocate）、
//! 目录的增删移，以及各页面之间的跳转。

use crate::{Level, Maintain, MergePreview, Page, Relocate, Resolve, ResolveRow, Rollback, RepoApp};
use crate::{daterange, svn, update};
use std::path::{Path, PathBuf};
use std::time::Instant;
use chrono::NaiveDate;
use crate::commit::CommitPage;
use crate::config::DirConfig;
use crate::history::{FileDiff, FileLog, HistoryPage, Zoom};
use crate::jobs::{Data, Kind};
use crate::repo::RepoKind;
use crate::svn::{LogEntry, LogPath};

impl RepoApp {
    // ------------------------------------------------------------ 后台任务

    pub fn spawn_detect(&mut self) {
        if self.pool.has(Kind::DetectSvn, usize::MAX) {
            return;
        }
        self.probing = true;
        self.refresh_after_detect = true;
        self.hint("正在自动寻找 svn.exe …");
        self.pool
            .spawn(Kind::DetectSvn, usize::MAX, "寻找 svn.exe".into(), move |sink| {
                sink.line("$ 自动寻找 svn.exe（环境变量 / PATH / 注册表 / 常见安装目录 / 磁盘浅层扫描）");
                let started = Instant::now();
                let found = svn::candidates();
                sink.line(format!(
                    "→ {} 个候选路径（扫描用时 {:.1} 秒）",
                    found.len(),
                    started.elapsed().as_secs_f32()
                ));
                let list: Vec<String> = found
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                match svn::detect(&found) {
                    Some((exe, version)) => Data::Svn {
                        exe: exe.to_string_lossy().into_owned(),
                        version,
                        candidates: list,
                    },
                    None => Data::Svn {
                        exe: String::new(),
                        version: String::new(),
                        candidates: list,
                    },
                }
            });
    }

    /// 当前选的更新源是否可用：两个 GitHub 源不需要任何配置，自建服务端源得填了服务根目录地址。
    pub fn update_source_ready(&self) -> bool {
        !update::is_custom(&self.cfg.update_source) || !self.cfg.update_server.trim().is_empty()
    }

    /// 后台检查一次版本更新（GitHub 最新发布 / GitHub 测试分支 / 自建服务端 latest.json，
    /// 与目录无关的任务）。地址有改动时会顺带保存配置。`quiet` 给定时自动检查用：过程不写日志区，
    /// 免得每 5 分钟刷两行、把真正的 svn 记录挤出输出区。
    pub fn spawn_update_check(&mut self, quiet: bool) {
        let custom = update::is_custom(&self.cfg.update_source);
        if custom && self.cfg.update_server.trim().is_empty() {
            self.hint("未配置更新服务端地址，请在「设置 → 版本更新」里填写");
            return;
        }
        self.persist();
        if self.pool.has(Kind::CheckUpdate, usize::MAX) {
            return;
        }
        let source = self.cfg.update_source.clone();
        let server = self.cfg.update_server.clone();
        let from = if custom { server.clone() } else { update::OFFICIAL_REPO.to_owned() };
        let channel = update::source_label(&source);
        self.pool
            .spawn(Kind::CheckUpdate, usize::MAX, "检查更新".into(), move |sink| {
                let sink = if quiet { sink.muted() } else { sink };
                sink.line(format!("$ 检查版本更新：{from}（{channel}）"));
                match update::check_from(&source, &server) {
                    Ok(manifest) => {
                        let test_tag = if manifest.prerelease { "（测试版）" } else { "" };
                        sink.line(format!(
                            "→ 最新版本：V{}{test_tag}",
                            manifest.version.trim()
                        ));
                        // 「有没有新版」按文件哈希判（要跑一次 certutil）：在后台这一趟算好，
                        // 界面那三处只读结论，别每帧重算
                        let ready = update::has_update(&manifest, crate::APP_VERSION);
                        sink.line(if ready {
                            "→ 更新源上的构建与本地不同：有可更新版本"
                        } else {
                            "→ 与本地运行的程序一致：已是最新"
                        });
                        Data::UpdateCheck {
                            ok: true,
                            message: String::new(),
                            info: Some(manifest),
                            ready,
                            quiet,
                        }
                    }
                    Err(e) => Data::UpdateCheck {
                        ok: false,
                        message: e,
                        info: None,
                        ready: false,
                        quiet,
                    },
                }
            });
    }

    /// 确认对话框里点了「开始更新」：后台下载新版本并校验，结果回来后再换上。
    pub fn begin_update(&mut self) {
        let Some(manifest) = self.update_info.clone() else {
            self.hint("还没有检查到新版本，请先「检查更新」");
            return;
        };
        if self.pool.has(Kind::DownloadUpdate, usize::MAX) {
            return;
        }
        let url = manifest.url.clone();
        let sha = manifest.sha256.trim().to_lowercase();
        self.pool.spawn(
            Kind::DownloadUpdate,
            usize::MAX,
            "下载更新".into(),
            move |sink| {
                sink.line(format!("$ 正在下载新版本：{url}"));
                let dest = std::env::temp_dir().join(update::STAGED_EXE);
                let _ = std::fs::remove_file(&dest);
                match update::download(&url, &dest) {
                    Ok(bytes) => {
                        sink.line(format!("→ 下载完成（{:.1} MB）", bytes as f64 / 1_048_576.0));
                        // 更新源给了校验值就核对（自建服务端的 sha256 字段、GitHub 资产的 digest），不一致直接删掉重来
                        if !sha.is_empty() {
                            match update::sha256_of(&dest) {
                                Ok(hash) if hash == sha => {
                                    sink.line("→ SHA256 校验通过");
                                }
                                Ok(hash) => {
                                    let _ = std::fs::remove_file(&dest);
                                    return Data::UpdateDownloaded {
                                        ok: false,
                                        message: format!(
                                            "SHA256 校验不符（期望 {sha}，实际 {hash}）"
                                        ),
                                        bytes,
                                    };
                                }
                                Err(e) => {
                                    let _ = std::fs::remove_file(&dest);
                                    return Data::UpdateDownloaded {
                                        ok: false,
                                        message: e,
                                        bytes,
                                    };
                                }
                            }
                        }
                        // 防御无限更新循环：更新源版本号升了但 exe 没换（或换回了同一个文件）
                        // 时，下载结果与当前程序完全相同，覆盖只会让程序反复重启「更新」
                        if let Ok(self_exe) = std::env::current_exe() {
                            match (update::sha256_of(&dest), update::sha256_of(&self_exe)) {
                                (Ok(new_hash), Ok(cur_hash)) if new_hash == cur_hash => {
                                    let _ = std::fs::remove_file(&dest);
                                    sink.line("→ 下载的文件与当前程序完全相同");
                                    return Data::UpdateDownloaded {
                                        ok: false,
                                        message: "下载的文件与当前程序完全相同：那次发布传的 exe 可能没有换成新构建，已取消覆盖".to_owned(),
                                        bytes,
                                    };
                                }
                                _ => {}
                            }
                        }
                        Data::UpdateDownloaded {
                            ok: true,
                            message: String::new(),
                            bytes,
                        }
                    }
                    Err(e) => Data::UpdateDownloaded {
                        ok: false,
                        message: e,
                        bytes: 0,
                    },
                }
            },
        );
    }

    /// 下载校验完成后：把新版换到正式名字下（正在运行的旧程序改名让路），再拉起新版并退出。
    ///
    /// 全程在程序内完成，不生成任何脚本：换名只在本目录内做（同卷的原子改名），路径不经过
    /// 控制台代码页，所以程序装在中文目录、exe 改成中文名都一样能更新。旧程序文件立刻删不掉
    /// （镜像还映射着），留给新版启动时的 `update::clean_leftovers` 收。
    pub(crate) fn apply_downloaded_update(&mut self) {
        let exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(e) => {
                let message = format!("无法确定程序自身路径：{e}");
                self.hint(message.clone());
                self.push(Level::Error, message);
                return;
            }
        };
        // 新版先落到程序目录：换名要求同一个卷，%TEMP% 可能挂在别的盘上
        let downloaded = std::env::temp_dir().join(update::STAGED_EXE);
        let staged = exe.with_file_name(update::STAGED_EXE);
        if let Err(e) = std::fs::copy(&downloaded, &staged) {
            let message = format!(
                "没法把新版 exe 放到程序目录（{}）：{e}；请手动下载新版本替换，或把它放到有写权限的目录再更新",
                staged.display()
            );
            self.hint(message.clone());
            self.push(Level::Error, message);
            return;
        }
        match update::swap_in_place(&exe, &staged) {
            Ok(old) => {
                self.push(
                    Level::Info,
                    format!(
                        "→ 新版本已就位：{}（旧程序改名到 {}，下次启动时清理）",
                        exe.display(),
                        old.display()
                    ),
                );
                match update::relaunch(&exe) {
                    Ok(()) => {
                        self.push(Level::Success, "程序即将退出，新版本会自动启动…");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        // 起不来就整个退回去：新文件收回暂存名、旧程序回到正式名，
                        // 本进程照旧跑着，用户重试一次即可
                        let _ = std::fs::rename(&exe, &staged);
                        let _ = std::fs::rename(&old, &exe);
                        self.hint(e.clone());
                        self.push(Level::Error, e);
                    }
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&staged);
                self.hint(e.clone());
                self.push(Level::Error, e);
            }
        }
    }

    /// 读一次本机 svn 登录人（`svn auth`），提交记录默认用它只看自己的提交。
    pub fn spawn_auth_user(&mut self) {
        let svn = self.svn.clone();
        let url = self
            .dirs
            .iter()
            .find_map(|view| view.info.clone())
            .map(|info| info.repos_root)
            .unwrap_or_default();
        // 等第一次检测出结果再查：有仓库地址才能挑出这台服务器的账号，
        // 否则会拿到 svn auth 里第一个凭据（可能是另一台服务器的另一个账号）
        let checked = self.dirs.iter().any(|view| view.remote.is_some());
        if self.user_probed
            || !self.svn.available()
            || self.pool.has(Kind::AuthUser, usize::MAX)
            || (url.is_empty() && !checked)
        {
            return;
        }
        self.user_probed = true;
        self.pool
            .spawn(Kind::AuthUser, usize::MAX, "本机 svn 登录人".to_owned(), move |sink| {
                sink.line("$ svn auth");
                let user = svn.auth_user(&url).unwrap_or_else(|| {
                    std::env::var("USERNAME")
                        .or_else(|_| std::env::var("USER"))
                        .unwrap_or_default()
                });
                sink.line(format!("→ 本机 svn 登录人：{user}"));
                Data::User { user }
            });
    }

    /// 本地开发侧（「更多 → 绑定对比对象」里绑的目录）与该目录工作副本的逐文件比对。
    /// 由 `spawn_refresh` 顺带起，所以自动刷新与提交 / 更新后的补刷都会带上这个数。
    pub fn spawn_local_diff(&mut self, index: usize, quiet: bool) {
        let target = self
            .cfg
            .dirs
            .get(index)
            .map(|d| d.bc_target.trim_matches('"').trim_matches(['/', '\\']).to_owned())
            .unwrap_or_default();
        if target.is_empty() {
            // 没绑定就没有「本地端」可谈：清掉可能残留的旧数，让主页什么都不显示
            if let Some(view) = self.dirs.get_mut(index) {
                view.local_diff = None;
            }
            return;
        }
        let (Some(server), label) = (self.dir_path(index), self.dir_label(index)) else { return };
        if self.pool.has(Kind::LocalDiff, index) || self.pool.is_writing(index) {
            // 正在 update / commit / 维护时工作副本树半新半旧，比出来的差异是假的。
            // 这些任务收尾都会补跑一次检测，那时候再起就对了
            return;
        }
        let filter = self.cfg.dirs[index].bc_filter.clone();
        self.pool
            .spawn(Kind::LocalDiff, index, format!("{label} 本地差异比对"), move |sink| {
                let sink = if quiet { sink.muted() } else { sink };
                match crate::localdiff::compare(Path::new(&target), &server, &filter) {
                    Ok(summary) => {
                        if !summary.is_clean() {
                            sink.line(format!("[{label}] {}", summary.label()));
                        }
                        Data::LocalDiff { dir: index, ok: true, summary }
                    }
                    Err(message) => {
                        sink.line(format!("[{label}] 本地差异比对失败：{message}"));
                        Data::LocalDiff { dir: index, ok: false, summary: Default::default() }
                    }
                }
            });
    }

    /// 导出本地差异诊断报告：把上一次比对的每一项判定重新读内容展开成文字报告
    /// （大小 / 修改时间 / 判成不同的原因 / 第一个差异字节 / 行摘录），写成文件
    /// 交给用户发回来——用户机器上才能看到的现象，靠这个文件带出来。
    pub fn export_local_diff_report(&mut self, index: usize, path: PathBuf) {
        let target = self
            .cfg
            .dirs
            .get(index)
            .map(|d| d.bc_target.trim_matches('"').trim_matches(['/', '\\']).to_owned())
            .unwrap_or_default();
        let (Some(server), label) = (self.dir_path(index), self.dir_label(index)) else { return };
        let filter = self.cfg.dirs[index].bc_filter.clone();
        self.pool
            .spawn(Kind::Diagnose, index, format!("{label} 导出差异报告"), move |sink| {
                sink.line("$ 导出本地差异诊断报告");
                let message = match crate::localdiff::diagnose(Path::new(&target), &server, &filter) {
                    Ok(report) => match std::fs::write(&path, report) {
                        Ok(()) => format!("诊断报告已写入：{}", path.display()),
                        Err(e) => format!("报告写入失败：{e}"),
                    },
                    Err(e) => e,
                };
                let ok = message.starts_with("诊断报告已写入");
                sink.line(format!("→ {message}"));
                Data::Run { dir: index, ok, message, reload: false }
            });
    }

    /// 打开双向合并窗口并起后台预览。备份清单在这里扫一次：上一次合并崩在半路时，
    /// 窗口顶部得立刻说得出「先恢复」，这次的清单才是叠在完整状态上算的
    pub fn open_merge(&mut self, index: usize) {
        // 入口在「更多」里已被收起，这里再挡一层，防止旧快捷键 / 测试路径绕过
        if !self.plugin_installed(crate::plugins::PluginId::Merge) {
            self.hint("「双向合并」插件没装：点顶栏的拼图按钮，在「插件」面板里导入 merge-plugin.dll");
            return;
        }
        let backups = crate::merge::backups(&crate::config::config_dir());
        // 双向合并看的是整份清单，默认就铺满工作区；点「还原大小」回默认尺寸
        self.merge = Some(MergePreview {
            dir: index,
            backups,
            zoom: crate::history::Zoom::maximized(),
            ..Default::default()
        });
        self.spawn_merge_plan(index);
    }

    /// 「更多 → 双向合并」的预览：读两边内容、往回扫历史定岔，算出每个文件的处置。
    /// 一个字节都不写，算完的清单交给合并窗口过目，落盘是 `spawn_merge_apply` 的事。
    pub fn spawn_merge_plan(&mut self, index: usize) {
        if self.dir_kind(index) == RepoKind::Git {
            self.hint("双向合并只针对 SVN 绑定的目录，Git 目录请用分支合并");
            return;
        }
        let (Some(svn), Some(server), Some(local)) =
            (self.svn_or_none(), self.dir_path(index), self.bound_local(index))
        else {
            return;
        };
        let label = self.dir_label(index);
        if self.pool.has(Kind::Merge, index) {
            self.hint("上一次合并预览还在跑，请待其结束后再试");
            return;
        }
        if self.pool.is_writing(index) {
            // 更新 / 提交 / 解决冲突跑着的时候，工作副本树半新半旧，定出来的岔是假的
            self.hint("该目录还有写操作在跑，等这一趟结束再算合并");
            return;
        }
        let filter = self.cfg.dirs[index].bc_filter.clone();
        let include = self.cfg.dirs[index].merge_include.clone();
        let exclude = self.cfg.dirs[index].merge_exclude.clone();
        let lcs_cell_limit = {
            let n = self.cfg.dirs[index].lcs_cell_limit;
            if n == 0 { crate::merge::LCS_CELL_LIMIT } else { n }
        };
        self.pool.spawn(Kind::Merge, index, format!("{label} 合并预览"), move |sink| {
            sink.line(format!(
                "$ 合并预览：本地开发侧 {} ↔ 服务器端 {}",
                local.display(),
                server.display()
            ));
            // 状态表读不出来的话，「这个文件是不是正冲突 / 已计划删除」就无从判断，
            // 整轮直接失败比逐条报「无基线」诚实；这件事现在由插件自己判断（它自己调 svn）
            let state_path = crate::merge::sync_state_path(&crate::config::config_dir(), &local, &server);
            match crate::merge::plan_with_state(
                &local,
                &server,
                &filter,
                &include,
                &exclude,
                Some(&svn),
                &crate::merge::MergeLimits {
                    lcs_cell_limit,
                    ..Default::default()
                },
                Some(&state_path),
            ) {
                Ok(plan) => {
                    if plan.binding_reset {
                        sink.line("→ SVN 仓库/分支或基线身份发生变化，已停用旧同步基线，需重新核对");
                    }
                    sink.line(format!(
                        "→ 合并预览：写入 {} 项 · 跳过 {} 项（冲突 {} · 待人工 {} · 不在范围 {}）",
                        plan.writing(),
                        plan.skipping(),
                        plan.conflicts(),
                        plan.undecided(),
                        plan.out_of_scope()
                    ));
                    if plan.budget_hit {
                        sink.line(format!("→ 预算用满，还有 {} 项没核对：这份清单不是全部", plan.untried));
                    }
                    Data::MergePlan { dir: index, ok: true, plan: Some(Box::new(plan)) }
                }
                Err(message) => {
                    sink.line(format!("→ 合并预览失败：{message}"));
                    Data::MergePlan { dir: index, ok: false, plan: None }
                }
            }
        });
    }

    /// 按预览清单落盘：`backup` 为真时两边整份备份到配置目录后再写，为假就是用户自己核对过的
    /// 「直接合并」，不留还原点。收尾补一次检测，让「本地端差异」和「待提交 N 项」跟着合并结果一起变
    pub fn spawn_merge_apply(&mut self, index: usize, plan: Box<crate::merge::Plan>, backup: bool) {
        let label = self.dir_label(index);
        if self.pool.is_writing(index) {
            self.hint("该目录还有写操作在跑，等它结束再落盘");
            return;
        }
        let root = crate::config::config_dir();
        self.pool
            .spawn(Kind::ApplyMerge, index, format!("{label} 应用合并"), move |sink| {
                sink.line(if backup {
                    format!("$ 备份到 {} 并按清单写入", root.display())
                } else {
                    "$ 按清单写入（不备份，本次不留还原点）".to_owned()
                });
                let result = crate::merge::apply(&plan, &root, backup).map(|done| {
                    (done.written, done.bytes, done.failed, done.backup.map(|d| d.display().to_string()))
                });
                merge_write_result(index, &label, "写入", result, &sink)
            });
    }

    /// 拿某一次备份还原：把合并写出去的内容整份退回备份时的样子
    pub fn spawn_merge_restore(&mut self, index: usize, backup: PathBuf) {
        let label = self.dir_label(index);
        if self.pool.is_writing(index) {
            self.hint("该目录还有写操作在跑，等它结束再还原");
            return;
        }
        let svn = self.svn_or_none();
        self.pool
            .spawn(Kind::ApplyMerge, index, format!("{label} 撤销合并"), move |sink| {
                sink.line(format!("$ 从 {} 还原", backup.display()));
                let result = crate::merge::restore_with_client(&backup, svn.as_ref())
                    .map(|done| {
                        (
                            done.restored + done.removed,
                            0_u64,
                            done.failed,
                            Some(backup.display().to_string()),
                        )
                    });
                merge_write_result(index, &label, "还原", result, &sink)
            });
    }

    /// 清掉配置目录里历次合并留下的备份。删的全是这台机器上的副本，两边文件一个字节不动，
    /// 但删完就没有还原点，所以确认在界面上走过一遍才到这里。备份攒起来可能几十兆，走后台。
    pub fn spawn_merge_clean(&mut self, index: usize) {
        let label = self.dir_label(index);
        let root = crate::config::config_dir();
        self.pool
            .spawn(Kind::CleanBackup, index, format!("{label} 清理备份"), move |sink| {
                sink.line(format!("$ 删除 {} 下的合并备份", root.display()));
                let (removed, bytes) = crate::merge::clean_backups(&root);
                let left = crate::merge::backups(&root)
                    .into_iter()
                    .filter(|item| !item.unfinished)
                    .count();
                let message = if left == 0 {
                    format!(
                        "{label}：已清掉 {removed} 次备份，释放 {}",
                        crate::merge::human_bytes(bytes)
                    )
                } else {
                    format!("{label}：还有 {left} 次备份没删掉，多半是目录正被别的程序占用")
                };
                sink.line(format!("→ {message}"));
                Data::Run { dir: index, ok: left == 0, message, reload: false }
            });
    }

    /// 「更多 → 绑定对比对象」里绑的那一侧（本地开发侧）。没绑就没有「本地端」可谈
    fn bound_local(&self, index: usize) -> Option<PathBuf> {
        let target = self
            .cfg
            .dirs
            .get(index)
            .map(|d| d.bc_target.trim_matches('"').trim_matches(['/', '\\']).to_owned())
            .unwrap_or_default();
        (!target.is_empty()).then(|| PathBuf::from(target))
    }

    /// `quiet` 为真时不在输出区打印执行日志（提交成功后的自动刷新用：那是程序
    /// 自己补的读操作，逐行 `$ svn …` 只会淹没提交结果；失败仍走结果消息报出来）。
    pub fn spawn_refresh(&mut self, index: usize, quiet: bool) {
        // 本地端比对不依赖 svn，所以放在 svn 就绪判断之前：svn.exe 还没配好时这个数照样能给
        self.spawn_local_diff(index, quiet);
        // Git 目录不需要 svn.exe（只装 git 的机器也要能用），所以先按后端分叉再查工具
        if self.dir_kind(index) == RepoKind::Git {
            self.spawn_refresh_git(index, quiet);
            return;
        }
        let Some(svn) = self.svn_or_none() else { return };
        let Some(path) = self.dir_path(index) else { return };
        if self.pool.has(Kind::Refresh, index) {
            // 直接返回的话，正在跑的那一次读的是提交前的旧数据，
            // 本地版本号就会一直停在旧值上，所以记一笔，等它落地后补跑
            if !self
                .pending_refresh
                .iter()
                .any(|(item, _)| *item == index)
            {
                self.pending_refresh.push((index, quiet));
            }
            return;
        }
        if !path.is_dir() {
            if let Some(view) = self.dirs.get_mut(index) {
                view.info = None;
                view.remote = Some(false);
                view.remote_msg = "目录不存在（可能已被移动或删除）".to_owned();
            }
            return;
        }
        let label = self.dir_label(index);
        // 主页那个「待提交 N」也归屏蔽规则管：这里少滤一次，就是屏幕上看不见却照样传上去
        let excludes = self
            .cfg
            .dirs
            .get(index)
            .map(|dir| dir.commit_excludes.clone())
            .unwrap_or_default();
        self.pool
            .spawn(Kind::Refresh, index, format!("{label} 检测"), move |sink| {
                let sink = if quiet { sink.muted() } else { sink };
                let target = path.to_string_lossy().into_owned();
                let (info, run) = svn.info(&target);
                let Some(info) = info else {
                    let message = run.summary();
                    sink.line(format!("[{label}] 不是 SVN 工作副本：{message}"));
                    return Data::Wc {
                        dir: index,
                        info: None,
                        remote: Some(false),
                        remote_msg: "不是 SVN 工作副本".to_owned(),
                        remote_rev: String::new(),
                        last_rev: String::new(),
                        last_author: String::new(),
                        last_date: String::new(),
                        changed: None,
                        changes: Vec::new(),
                        all_changes: Vec::new(),
                        conflicts: None,
                        blocked: Vec::new(),
                        out_of_date: None,
                    };
                };
                let mut remote = Some(true);
                let mut remote_msg = "已连接到仓库".to_owned();
                let mut remote_rev = String::new();
                let mut last_rev = String::new();
                let mut last_author = String::new();
                let mut last_date = String::new();
                if info.url.is_empty() {
                    remote = Some(false);
                    remote_msg = "无法取得仓库 URL".to_owned();
                } else {
                    sink.line(format!("$ svn info --xml {}", info.url));
                    let (head, probe) = svn.info(&info.url);
                    if probe.ok {
                        if let Some(head) = head {
                            remote_rev = head.revision;
                            last_rev = head.last_rev;
                            last_author = head.last_author;
                            last_date = head.last_date;
                        }
                    } else {
                        remote = Some(false);
                        remote_msg = probe.summary();
                    }
                }
                // 服务器可达时一次走完整棵树就够：`svn status -u --xml` 的输出同时带
                // 本地改动（wc-status）与服务器上已变更的条目（repos-status）。
                // 早先分两条命令等于把树走两遍，七万文件的目录要多等三秒。
                // 再省一层：本地基线已经就是 HEAD 时，`-u` 一路必然全空，那半趟走完整棵树
                // 的服务器比较（同一个目录实测 0.6~3.4 秒）就不用问了，只读本地状态。
                let (entries, out_of_date, status) = if remote == Some(true) {
                    if svn::wc_at_head(&info.revision, &remote_rev) {
                        let (local, local_run) = svn.status(&path);
                        (local, Some(0), local_run)
                    } else {
                        let (entries, pending, run) = svn.status_against_server(&path);
                        if run.ok {
                            (entries, pending, run)
                        } else {
                            // 带 -u 的这次没走通（服务器中途断了 / 认证问题）：退回只读本地
                            let (local, local_run) = svn.status(&path);
                            (local, None, local_run)
                        }
                    }
                } else {
                    // 连接已经判定不通，就别再让那条走网络的命令卡到超时
                    let (local, local_run) = svn.status(&path);
                    (local, None, local_run)
                };
                // 统计口径与提交页、「全部上传」共用一个函数：? 会在提交时自动 add、! 自动 delete，
                // 所以它们也算待提交项；本目录屏蔽规则挡掉的那些既看不见也不提交
                let changes: Vec<crate::svn::StatusEntry> =
                    crate::commit::pending_entries(&entries, &path, &excludes);
                let changed = status.ok.then(|| changes.len());
                // 冲突数在未过滤的 entries 上数：changes 里已经没有冲突条目了。
                // status 读失败只能给 None——给 0 会把「没读到」显示成「没有冲突」
                let conflicts = status.ok.then(|| crate::svn::blocked_count(&entries));
                if let Some(pending) = out_of_date.filter(|count| *count > 0) {
                    sink.line(format!("[{label}] 服务器上有 {pending} 项本地还没更新"));
                }
                if remote != Some(true) {
                    sink.line(format!("[{label}] 连接异常：{remote_msg}"));
                }
                Data::Wc {
                    dir: index,
                    info: Some(info),
                    remote,
                    remote_msg,
                    remote_rev,
                    last_rev,
                    last_author,
                    last_date,
                    changed,
                    changes,
                    all_changes: entries.clone(),
                    conflicts,
                    blocked: crate::svn::blocked_entries(&entries),
                    out_of_date,
                }
            });
    }

    pub fn spawn_all_refresh(&mut self) {
        for index in 0..self.cfg.dirs.len() {
            self.spawn_refresh(index, false);
        }
    }

    /// Git 目录的检测：读本地状态，fetch 后比出落后远端几个提交。
    /// 口径与 svn 的检测一致——fetch 失败时可更新数给 None（不能宣称「已是最新」），
    /// 没配远端 / 分支还没推过则按落后 0 个提交算；上游 tip 的最后提交当「服务器最后提交」。
    fn spawn_refresh_git(&mut self, index: usize, quiet: bool) {
        let Some(git) = self.git_or_none() else { return };
        let Some(path) = self.dir_path(index) else { return };
        if self.pool.has(Kind::Refresh, index) {
            // 同 svn：正在跑的那一次读的是提交前的旧数据，落地后补跑一次
            if !self
                .pending_refresh
                .iter()
                .any(|(item, _)| *item == index)
            {
                self.pending_refresh.push((index, quiet));
            }
            return;
        }
        if !path.is_dir() {
            if let Some(view) = self.dirs.get_mut(index) {
                view.info = None;
                view.remote = Some(false);
                view.remote_msg = "目录不存在（可能已被移动或删除）".to_owned();
            }
            return;
        }
        let label = self.dir_label(index);
        let excludes = self
            .cfg
            .dirs
            .get(index)
            .map(|dir| dir.commit_excludes.clone())
            .unwrap_or_default();
        self.pool
            .spawn(Kind::Refresh, index, format!("{label} 检测"), move |sink| {
                let sink = if quiet { sink.muted() } else { sink };
                let (info, info_run) = git.info(&path);
                let Some(info) = info else {
                    sink.line(format!("[{label}] 不是 Git 仓库：{}", info_run.summary()));
                    return Data::Wc {
                        dir: index,
                        info: None,
                        remote: Some(false),
                        remote_msg: "不是 Git 仓库".to_owned(),
                        remote_rev: String::new(),
                        last_rev: String::new(),
                        last_author: String::new(),
                        last_date: String::new(),
                        changed: None,
                        changes: Vec::new(),
                        all_changes: Vec::new(),
                        conflicts: None,
                        blocked: Vec::new(),
                        out_of_date: None,
                    };
                };
                sink.line(format!(
                    "$ git status --porcelain=v2 --untracked-files=all \"{}\"",
                    path.display()
                ));
                let (entries, out_of_date, tip, probe_run) = git.status_against_server(&path);
                let mut remote = Some(true);
                let mut remote_msg = "已连接到仓库".to_owned();
                if !probe_run.ok {
                    remote = Some(false);
                    remote_msg = probe_run.summary();
                } else if tip.is_empty() {
                    // fetch 没报错但没有上游：没配远端（或分支还没推过），只看本地
                    remote_msg = "未配置远端（或分支还没建立上游）".to_owned();
                } else {
                    sink.line("$ git fetch --quiet && git rev-list --count HEAD..@{upstream}");
                }
                // 远端这趟没走通时本地状态未必也挂了（多半是 fetch 断网）：照 svn 的口径
                // 退回只读本地，待提交 / 冲突数以本地这一次为准，不因为断网就清零
                let (entries, status_ok) = if probe_run.ok {
                    (entries, true)
                } else {
                    let (local, local_run) = git.status(&path);
                    (if local_run.ok { local } else { entries }, local_run.ok)
                };
                match out_of_date {
                    Some(0) => {}
                    Some(pending) => {
                        sink.line(format!("[{label}] 远端比本地多 {pending} 个提交，点「更新」拉下来"));
                    }
                    None => {}
                }
                // 「服务器最后提交」取上游 tip 的；没有远端时退回本地 HEAD 的
                let (mut last_rev, mut last_author, mut last_date) = (
                    info.last_rev.clone(),
                    info.last_author.clone(),
                    info.last_date.clone(),
                );
                if !tip.is_empty() {
                    let (tip_rev, tip_author, tip_date) = git.tip_meta(&path, &tip);
                    if !tip_rev.is_empty() {
                        last_rev = tip_rev;
                        last_author = tip_author;
                        last_date = tip_date;
                    }
                }
                // 统计口径与提交页、「全部上传」共用一个函数：? 会在提交时自动 add、! 自动 delete
                let changes: Vec<crate::svn::StatusEntry> =
                    crate::commit::pending_entries(&entries, &path, &excludes);
                let changed = status_ok.then(|| changes.len());
                // 冲突数在未过滤的 entries 上数：changes 里已经没有冲突条目了。
                // status 读失败只能给 None——给 0 会把「没读到」显示成「没有冲突」
                let conflicts = status_ok.then(|| crate::svn::blocked_count(&entries));
                if remote != Some(true) {
                    sink.line(format!("[{label}] 连接异常：{remote_msg}"));
                }
                Data::Wc {
                    dir: index,
                    info: Some(info),
                    remote,
                    remote_msg,
                    remote_rev: tip,
                    last_rev,
                    last_author,
                    last_date,
                    changed,
                    changes,
                    all_changes: entries.clone(),
                    conflicts,
                    blocked: crate::svn::blocked_entries(&entries),
                    out_of_date,
                }
            });
    }

    pub fn spawn_update(&mut self, index: usize) {
        let Some(path) = self.dir_path(index) else { return };
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            let label = self.dir_label(index);
            self.pool
                .spawn(Kind::Update, index, format!("{label} 更新"), move |sink| {
                    sink.line(format!("$ git pull --ff-only \"{}\"", path.display()));
                    let run = git.update(&path, &|line| sink.line(line));
                    let message = if run.ok {
                        format!("{label}：更新完成")
                    } else {
                        format!("{label}：更新失败——{}", run.summary())
                    };
                    // 收尾文案只交给任务回收端统一写进输出区，避免同一句显示两遍
                    Data::Run {
                        dir: index,
                        ok: run.ok,
                        message,
                        reload: true,
                    }
                });
            return;
        }
        let Some(svn) = self.svn_or_none() else { return };
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Update, index, format!("{label} 更新"), move |sink| {
                sink.line(format!("$ svn update \"{}\"", path.display()));
                let run = svn.update(&path, &|line| sink.line(line));
                let message = if run.ok {
                    format!("{label}：更新完成")
                } else {
                    format!("{label}：更新失败——{}", run.summary())
                };
                // 收尾文案只交给任务回收端统一写进输出区，避免同一句显示两遍
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message,
                    reload: true,
                }
            });
    }

    pub fn spawn_status(&mut self, index: usize, quiet: bool) {
        let Some(path) = self.dir_path(index) else { return };
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            if self.pool.has(Kind::Status, index) {
                return;
            }
            let label = self.dir_label(index);
            self.pool
                .spawn(Kind::Status, index, format!("{label} 读取修改"), move |sink| {
                    // 静音时丢弃执行日志，读失败仍通过 Data::Status 的 message 报给界面
                    let sink = if quiet { sink.muted() } else { sink };
                    sink.line(format!(
                        "$ git status --porcelain=v2 --untracked-files=all \"{}\"",
                        path.display()
                    ));
                    let (entries, run) = git.status(&path);
                    sink.line(format!("→ 共 {} 项改动", entries.len()));
                    Data::Status {
                        dir: index,
                        entries,
                        ok: run.ok,
                        message: if run.ok { String::new() } else { run.summary() },
                    }
                });
            return;
        }
        let Some(svn) = self.svn_or_none() else { return };
        if self.pool.has(Kind::Status, index) {
            return;
        }
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Status, index, format!("{label} 读取修改"), move |sink| {
                // 静音时丢弃执行日志，读失败仍通过 Data::Status 的 message 报给界面
                let sink = if quiet { sink.muted() } else { sink };
                sink.line(format!("$ svn status --xml \"{}\"", path.display()));
                let (entries, run) = svn.status(&path);
                sink.line(format!("→ 共 {} 项改动", entries.len()));
                Data::Status {
                    dir: index,
                    entries,
                    ok: run.ok,
                    message: if run.ok { String::new() } else { run.summary() },
                }
            });
    }

    /// 读一个目录的提交记录。`mine` 为真时只取本机 svn 登录人的记录（`svn log --search`）；
    /// `unlimited` 为真时临时不带 `-l`（不限条数拉全量），只影响这一次读取，不写设置。
    /// `range` 为 `Some` 时按日期区间读服务器（`svn log -r {止}:{起}`，新 → 旧）：
    /// 区间模式下条数没有意义（要的是那几天，不是最近 N 条），所以不带 `-l`；
    /// 目标串还要显式 peg 到 `@HEAD`：工作副本的隐式 peg 是 BASE，本地落后时按日期找版本会取不到新提交。
    pub fn spawn_log_in(
        &mut self,
        index: usize,
        mine: bool,
        unlimited: bool,
        range: Option<(NaiveDate, NaiveDate)>,
    ) {
        let Some(path) = self.dir_path(index) else { return };
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            if self.pool.has(Kind::Log, index) {
                return;
            }
            // 区间模式下条数没有意义（要的是那几天，不是最近 N 条）
            let limit = match range {
                Some(_) => 0,
                None if unlimited => 0,
                None => self.cfg.log_limit.clamp(1, 2000),
            };
            let (since, until) = match range {
                Some((from, to)) => (
                    from.format("%Y-%m-%d").to_string(),
                    // git 的 --until 停在那天零点之前，加一天才把结束当天整天包进来
                    to.succ_opt().unwrap_or(to).format("%Y-%m-%d").to_string(),
                ),
                None => (String::new(), String::new()),
            };
            let label = self.dir_label(index);
            self.pool
                .spawn(Kind::Log, index, format!("{label} 提交记录"), move |sink| {
                    // git 没有 svn auth：提交人取仓库配置的 user.name，取不到就不筛人
                    let author = if mine {
                        git.auth_user(&path).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let limit_part = if limit > 0 {
                        format!(" -n {limit}")
                    } else if range.is_none() {
                        "（不限条数）".to_owned()
                    } else {
                        String::new()
                    };
                    let range_part = if since.is_empty() {
                        String::new()
                    } else {
                        format!(" --since={since} --until={until}")
                    };
                    let author_part = if author.is_empty() {
                        String::new()
                    } else {
                        format!(" --author={author}")
                    };
                    sink.line(format!(
                        "$ git log --name-status{limit_part}{range_part}{author_part} \"{path_display}\"",
                        path_display = path.display()
                    ));
                    let (entries, run) = git.log_in(&path, &since, &until, limit, &author);
                    sink.line(format!("→ 共 {} 条提交记录", entries.len()));
                    Data::Log {
                        dir: index,
                        entries,
                        ok: run.ok,
                        message: if run.ok { String::new() } else { run.summary() },
                    }
                });
            return;
        }
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        if self.pool.has(Kind::Log, index) {
            return;
        }
        // limit 传 0 表示不限条数（svn.log_in 里据此省掉 -l）；按区间读时固定不限
        let limit = match range {
            Some(_) => 0,
            None if unlimited => 0,
            None => self.cfg.log_limit.clamp(1, 2000),
        };
        let spec = daterange::revspec(range);
        let author = if mine { self.svn_user.clone() } else { String::new() };
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Log, index, format!("{label} 提交记录"), move |sink| {
                let shown = if author.is_empty() {
                    String::new()
                } else {
                    format!(" --search {author}")
                };
                let limit_part = match range {
                    Some(_) => String::new(),
                    None if limit > 0 => format!(" -l {limit}"),
                    None => "（不限条数）".to_owned(),
                };
                let target = match range {
                    Some(_) => format!("{}@HEAD", path.to_string_lossy()),
                    None => path.to_string_lossy().into_owned(),
                };
                sink.line(format!(
                    "$ svn log -v -r {spec}{limit_part}{shown} --xml \"{target}\""
                ));
                let (entries, run) = svn.log_in(&target, &spec, limit, &author);
                sink.line(format!("→ 共 {} 条提交记录", entries.len()));
                Data::Log {
                    dir: index,
                    entries,
                    ok: run.ok,
                    message: if run.ok { String::new() } else { run.summary() },
                }
            });
    }

    /// 双击历史页的「涉及文件」：看这一次提交对这个文件 / 目录改动了哪几行（和上一个版本比）。
    /// `path.path` 是 svn log 返回的路径，相对仓库根（如 `/code/HRP.FA/...`），拼上 repos_root 就能查。
    pub fn open_file_diff(&mut self, index: usize, entry: &LogEntry, path: &LogPath) {
        // 一次只读一条：上一条还没回来就换路径，结果会串到新的路径上
        if self.pool.has(Kind::FileDiff, index) {
            self.hint("上一条差异还在读取，稍等再双击");
            return;
        }
        if self.dir_kind(index) == RepoKind::Git {
            // git 不需要仓库根地址：「url」直接存仓库相对路径，本地文件被改名 / 删除了照样按 pathspec 查
            let revision = entry.revision.clone();
            // XML 整份被改缩进 / 换行符的情况最多，这类文件默认忽略空白；其余文件默认看原始差异
            let ignore_white = path.path.to_ascii_lowercase().ends_with(".xml");
            let rev = crate::repo::fmt_rev(self.dir_kind(index), &revision);
            self.hint(format!("正在读取 {rev} 对 {} 的逐行改动 …", path.path));
            self.file_diff = Some(FileDiff {
                dir: index,
                revision: revision.clone(),
                author: entry.author.clone(),
                date: entry.date.clone(),
                message: entry.message.clone(),
                path: path.path.clone(),
                action: path.action,
                url: path.path.clone(),
                diff: String::new(),
                error: String::new(),
                ignore_white,
                // 改动明细默认最大化：看差异要的是横向空间，点「还原」才回到原来的大小
                zoom: Zoom::maximized(),
            });
            self.spawn_file_diff(index, revision, path.path.clone(), ignore_white);
            return;
        }
        let root = self
            .dirs
            .get(index)
            .and_then(|view| view.info.clone())
            .map(|info| info.repos_root)
            .unwrap_or_default();
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            self.hint("还没读到该目录的仓库根地址，无法查看逐行改动");
            return;
        }
        let mut url = root.to_owned();
        if !path.path.starts_with('/') {
            url.push('/');
        }
        url.push_str(&path.path);
        let revision = entry.revision.clone();
        // XML 整份被改缩进 / 换行符的情况最多，这类文件默认忽略空白；其余文件默认看原始差异
        let ignore_white = path.path.to_ascii_lowercase().ends_with(".xml");
        self.hint(format!("正在读取 r{revision} 对 {} 的逐行改动 …", path.path));
        self.file_diff = Some(FileDiff {
            dir: index,
            revision: revision.clone(),
            author: entry.author.clone(),
            date: entry.date.clone(),
            message: entry.message.clone(),
            path: path.path.clone(),
            action: path.action,
            url: url.clone(),
            diff: String::new(),
            error: String::new(),
            ignore_white,
            // 改动明细默认最大化：看差异要的是横向空间，点「还原」才回到原来的大小
            zoom: Zoom::maximized(),
        });
        self.spawn_file_diff(index, revision, url, ignore_white);
    }

    /// `svn diff -c <版本> <仓库URL>`：这一次提交相对上一个版本，对该路径改了哪几行。
    /// 用仓库 URL 查，本地没有该文件（或已被改名 / 删除）也能取到；本次新增的文件会整份显示为 +。
    pub fn spawn_file_diff(&mut self, index: usize, revision: String, url: String, ignore_white: bool) {
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            let Some(dir) = self.dir_path(index) else { return };
            if self.pool.has(Kind::FileDiff, index) {
                self.hint("正在读取上一条差异，请待完成后重新点击「重新读取」");
                return;
            }
            let rev = crate::repo::fmt_rev(self.dir_kind(index), &revision);
            let shown = url.rsplit('/').next().unwrap_or("").to_owned();
            self.pool.spawn(
                Kind::FileDiff,
                index,
                format!("{shown} {rev} 差异"),
                move |sink| {
                    let flags = if ignore_white {
                        format!(" {}", crate::git::DIFF_IGNORE_ARGS.join(" "))
                    } else {
                        String::new()
                    };
                    sink.line(format!(
                        "$ git diff {revision}~1 {revision} --no-ext-diff{flags} \":(top){url}\""
                    ));
                    let run = git.diff_rev(&dir, &revision, &url, ignore_white);
                    sink.line(format!(
                        "→ {}",
                        if run.ok { "已取到差异".to_owned() } else { run.summary() }
                    ));
                    Data::Run {
                        dir: index,
                        ok: run.ok,
                        message: if run.ok { run.out } else { run.summary() },
                        reload: false,
                    }
                },
            );
            return;
        }
        let Some(svn) = self.svn_or_none() else {
            self.hint("没有可用的 svn.exe，无法读取差异");
            return;
        };
        if self.pool.has(Kind::FileDiff, index) {
            self.hint("正在读取上一条差异，请待完成后重新点击「重新读取」");
            return;
        }
        let shown = url.rsplit('/').next().unwrap_or("").to_owned();
        self.pool.spawn(
            Kind::FileDiff,
            index,
            format!("{shown} r{revision} 差异"),
            move |sink| {
                let flags = crate::svn::diff_flags(ignore_white);
                sink.line(format!("$ svn diff -c {revision} --internal-diff{flags} \"{url}\""));
                let run = svn.diff_rev(&revision, &url, ignore_white);
                sink.line(format!(
                    "→ {}",
                    if run.ok { "已取到差异".to_owned() } else { run.summary() }
                ));
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message: if run.ok { run.out } else { run.summary() },
                    reload: false,
                }
            },
        );
    }
    /// 点涉及文件行右侧的「查看提交记录」：查这个文件 / 目录自己在服务器上的提交记录。
    /// 地址 = 仓库根 + log 返回的相对路径（相对仓库根，如 `/code/HRP.FA/...`）。
    pub fn open_file_log(&mut self, index: usize, path: &LogPath) {
        // 一次只读一条：上一条没回来就换路径，结果会串到别的路径上
        if self.pool.has(Kind::FileLog, index) {
            self.hint("正在读取上一个文件的提交记录，请稍候再试");
            return;
        }
        if self.dir_kind(index) == RepoKind::Git {
            // git 的「url」就是仓库相对路径：按 pathspec 查历史，不依赖仓库根地址
            let limit = self.cfg.log_limit.clamp(1, 2000);
            self.hint(format!("正在读取 {} 的提交记录 …", path.path));
            self.file_log = Some(FileLog {
                dir: index,
                url: path.path.clone(),
                name: path.path.clone(),
                limit,
                entries: Vec::new(),
                error: String::new(),
                zoom: Zoom::default(),
            });
            self.spawn_file_log(index, path.path.clone(), limit);
            return;
        }
        let root = self
            .dirs
            .get(index)
            .and_then(|view| view.info.clone())
            .map(|info| info.repos_root)
            .unwrap_or_default();
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            self.hint("还没读到该目录的仓库根地址，无法查询单个文件的提交记录");
            return;
        }
        let mut url = root.to_owned();
        if !path.path.starts_with('/') {
            url.push('/');
        }
        url.push_str(&path.path);
        let limit = self.cfg.log_limit.clamp(1, 2000);
        self.hint(format!("正在读取 {} 的提交记录 …", path.path));
        self.file_log = Some(FileLog {
            dir: index,
            url: url.clone(),
            name: path.path.clone(),
            limit,
            entries: Vec::new(),
            error: String::new(),
            zoom: Zoom::default(),
        });
        self.spawn_file_log(index, url, limit);
    }

    /// `svn log <仓库URL>`：只返回改动过这个路径的那些版本（同样一律读服务器）。
    pub fn spawn_file_log(&mut self, index: usize, url: String, limit: i64) {
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            let Some(dir) = self.dir_path(index) else { return };
            if self.pool.has(Kind::FileLog, index) {
                self.hint("正在读取上一个文件的提交记录，请稍候再试");
                return;
            }
            let shown = url.rsplit('/').next().unwrap_or("").to_owned();
            self.pool
                .spawn(Kind::FileLog, index, format!("{shown} 提交记录"), move |sink| {
                    sink.line(format!("$ git log --name-status -- \":(top){url}\""));
                    let (entries, run) = git.log_path(&dir, &url, limit, "");
                    sink.line(format!("→ 共 {} 条记录", entries.len()));
                    Data::Log {
                        dir: index,
                        entries,
                        ok: run.ok,
                        message: if run.ok { String::new() } else { run.summary() },
                    }
                });
            return;
        }
        let Some(svn) = self.svn_or_none() else {
            self.hint("没有可用的 svn.exe，无法读取文件提交记录");
            return;
        };
        if self.pool.has(Kind::FileLog, index) {
            self.hint("正在读取上一个文件的提交记录，请稍候再试");
            return;
        }
        let shown = url.rsplit('/').next().unwrap_or("").to_owned();
        self.pool.spawn(
            Kind::FileLog,
            index,
            format!("{shown} 提交记录"),
            move |sink| {
                sink.line(format!("$ svn log -v -r HEAD:1 -l {limit} --xml \"{url}\""));
                let (entries, run) = svn.log(&url, limit, "");
                sink.line(format!("→ 共 {} 条记录", entries.len()));
                Data::Log {
                    dir: index,
                    entries,
                    ok: run.ok,
                    message: if run.ok { String::new() } else { run.summary() },
                }
            },
        );
    }
    pub fn spawn_diff(&mut self, index: usize, path: String, ignore_white: bool) {
        if self.dir_kind(index) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            let target = PathBuf::from(&path);
            self.pool
                .spawn(Kind::Diff, index, "查看差异".into(), move |sink| {
                    let flags = if ignore_white {
                        format!(" {}", crate::git::DIFF_IGNORE_ARGS.join(" "))
                    } else {
                        String::new()
                    };
                    sink.line(format!("$ git diff HEAD --no-ext-diff{flags} \"{path}\""));
                    let run = git.diff(&target, ignore_white);
                    Data::Run {
                        dir: index,
                        ok: run.ok,
                        message: if run.ok { run.out } else { run.summary() },
                        reload: false,
                    }
                });
            return;
        }
        let Some(svn) = self.svn_or_none() else {
            self.hint("没有可用的 svn.exe，无法读取差异");
            return;
        };
        let target = PathBuf::from(&path);
        self.pool
            .spawn(Kind::Diff, index, "查看差异".into(), move |sink| {
                let flags = crate::svn::diff_flags(ignore_white);
                sink.line(format!("$ svn diff --internal-diff{flags} \"{path}\""));
                let run = svn.diff(&target, ignore_white);
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message: if run.ok { run.out } else { run.summary() },
                    reload: false,
                }
            });
    }

    pub fn spawn_maintain(&mut self, index: usize, op: Maintain) {
        if self.dir_kind(index) == RepoKind::Git {
            self.spawn_maintain_git(index, op);
            return;
        }
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        let label = self.dir_label(index);
        let targets: Vec<String> = match op {
            Maintain::Add | Maintain::Delete => {
                let list = self
                    .commit
                    .as_ref()
                    .map(|commit| commit.selected_targets(op))
                    .unwrap_or_default();
                if list.is_empty() {
                    self.hint(format!("请先勾选需要「{}」的条目", op.label()));
                    return;
                }
                list
            }
            _ => Vec::new(),
        };
        if op == Maintain::Revert {
            self.hint("已撤销该目录下的全部本地修改");
        }
        let count = targets.len();
        let sub = match op {
            Maintain::Add => "add --force",
            Maintain::Delete => "delete",
            Maintain::Cleanup => "cleanup",
            Maintain::Revert => "revert --recursive",
        };
        self.pool
            .spawn(Kind::Maintain, index, format!("{label} {sub}"), move |sink| {
                if count > 0 {
                    sink.line(format!("$ svn {sub} （{count} 项）"));
                } else {
                    sink.line(format!("$ svn {sub} \"{}\"", path.display()));
                }
                let run = match op {
                    Maintain::Add => svn.add(&path, &targets, &|line| sink.line(line)),
                    Maintain::Delete => svn.delete(&path, &targets, &|line| sink.line(line)),
                    Maintain::Cleanup => svn.cleanup(&path, &|line| sink.line(line)),
                    Maintain::Revert => svn.revert_all(&path, &|line| sink.line(line)),
                };
                let message = if run.ok {
                    format!("{label}：{} 完成", op.label())
                } else {
                    format!("{label}：{} 失败——{}", op.label(), run.summary())
                };
                // 收尾文案只交给任务回收端统一写进输出区，避免同一句显示两遍
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message,
                    reload: op != Maintain::Cleanup,
                }
            });
    }

    /// Git 目录的维护操作：加 / 删 / 撤销三项照常，清理与解决冲突是 SVN 特有的概念。
    fn spawn_maintain_git(&mut self, index: usize, op: Maintain) {
        let sub = match op {
            Maintain::Cleanup => {
                self.hint(format!("Git 仓库用不到「{}」", op.label()));
                return;
            }
            Maintain::Add => "add -A",
            Maintain::Delete => "rm -r",
            Maintain::Revert => "reset + restore",
        };
        let (Some(git), Some(path)) = (self.git_or_none(), self.dir_path(index)) else {
            return;
        };
        let targets: Vec<String> = match op {
            Maintain::Add | Maintain::Delete => {
                let list = self
                    .commit
                    .as_ref()
                    .map(|commit| commit.selected_targets(op))
                    .unwrap_or_default();
                if list.is_empty() {
                    self.hint(format!("请先勾选需要「{}」的条目", op.label()));
                    return;
                }
                list
            }
            _ => Vec::new(),
        };
        if op == Maintain::Revert {
            self.hint("已撤销该目录下的全部本地修改");
        }
        let count = targets.len();
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Maintain, index, format!("{label} {sub}"), move |sink| {
                if count > 0 {
                    sink.line(format!("$ git {sub}（{count} 项）"));
                } else {
                    sink.line(format!("$ git {sub} \"{}\"", path.display()));
                }
                let run = match op {
                    Maintain::Add => git.add(&path, &targets, &|line| sink.line(line)),
                    Maintain::Delete => git.delete(&path, &targets, &|line| sink.line(line)),
                    Maintain::Revert => git.revert_all(&path, &|line| sink.line(line)),
                    // 上面已经拦掉，给个空结果只为让 match 完整
                    Maintain::Cleanup => crate::svn::Run {
                        ok: true,
                        code: 0,
                        out: String::new(),
                        err: String::new(),
                    },
                };
                let message = if run.ok {
                    format!("{label}：{} 完成", op.label())
                } else {
                    format!("{label}：{} 失败——{}", op.label(), run.summary())
                };
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message,
                    reload: op != Maintain::Cleanup,
                }
            });
    }

    /// 打开「解决冲突」窗口。条目取自最近一次读到的 status（口径同「冲突 N 项」），
    /// 开完再补一趟 status 把列表核正——窗口开着时他可能已经去 BC 里合完了。
    pub fn open_resolve(&mut self, index: usize) {
        if self.svn_or_none().is_none() {
            return;
        }
        let Some(view) = self.dirs.get(index) else { return };
        if view.conflicts.is_none() {
            self.hint("还没读到这个目录的状态，先点「全部检测」");
            return;
        }
        if view.conflicts == Some(0) {
            self.hint("这个目录没有需要人工处理的冲突条目");
            return;
        }
        self.resolve = Some(Resolve::from_entries(index, &view.blocked));
        self.spawn_status(index, true);
    }

    /// 按窗口里逐行挑好的策略执行：同一条策略的路径并成一条命令发给 svn。
    /// 只动本地工作副本，不碰服务器；跑完照例重读 status 与检测。
    pub fn spawn_resolve(&mut self, index: usize, rows: Vec<ResolveRow>) {
        let Some(svn) = self.svn_or_none() else { return };
        if rows.is_empty() {
            self.hint("请先勾选要处理的条目");
            return;
        }
        if self.pool.has(Kind::Resolve, index) {
            self.hint("该目录的上一次解决冲突正在执行，请待其结束后再试");
            return;
        }
        let label = self.dir_label(index);
        let total = rows.len();
        self.pool
            .spawn(Kind::Resolve, index, format!("{label} 解决冲突"), move |sink| {
                let mut failed: Vec<String> = Vec::new();
                for accept in crate::svn::Accept::ALL {
                    let targets: Vec<PathBuf> = rows
                        .iter()
                        .filter(|row| row.accept == accept)
                        .map(|row| row.target.clone())
                        .collect();
                    if targets.is_empty() {
                        continue;
                    }
                    sink.line(format!("$ svn {}（{} 项）", accept.command(), targets.len()));
                    let run = svn.accept_conflicts(accept, &targets, &|line| sink.line(line));
                    if !run.ok {
                        failed.push(format!("「{}」：{}", accept.label(), run.summary()));
                    }
                }
                let message = if failed.is_empty() {
                    format!("{label}：{total} 项已按所选策略处理")
                } else {
                    format!("{label}：解决冲突有失败——{}", failed.join("；"))
                };
                Data::Run {
                    dir: index,
                    ok: failed.is_empty(),
                    message,
                    reload: true,
                }
            });
    }

    /// 打开「回退到 r{版本}」确认窗口。`path` 不给 = 整个目录回到这一次提交的样子；
    /// 给了就是历史页「涉及文件」里的那一个文件，只回退它。
    /// 两种都只改本地工作副本，确认窗口里讲清楚后果才执行。
    pub fn open_rollback(&mut self, index: usize, entry: &LogEntry, path: Option<&LogPath>) {
        let (Some(info), Some(local)) = (
            self.dirs.get(index).and_then(|view| view.info.clone()),
            self.dir_path(index),
        ) else {
            self.hint("还没读到该目录的仓库地址，请先回主页「全部检测」");
            return;
        };
        if self.pool.has(Kind::Rollback, index) {
            self.hint("该目录的上一次回退正在执行，请待其结束后再试");
            return;
        }
        // 冲突不挡回退：svn merge 带 --accept postpone，冲突出来的照样能收尾。
        // 但要把数记下来，确认窗口里说清回退会把它们一起卷进来
        let blocked = self
            .dirs
            .get(index)
            .and_then(|view| view.conflicts)
            .filter(|count| *count > 0)
            .unwrap_or_default();
        if self.dir_kind(index) == RepoKind::Git {
            self.open_rollback_git(index, entry, path, local, blocked);
            return;
        }
        let (target, name, is_dir) = match path {
            None => (local, self.dir_label(index), true),
            Some(item) => {
                // 日志里的路径相对仓库根，要先换算成本地这个目录下的真实文件
                let Some(target) = svn::wc_target(&info.repos_root, &info.url, &local, &item.path)
                else {
                    self.hint(format!(
                        "{} 不在本目录的本地工作副本里（那次提交还动了同一个仓库的别的路径），这里回退不了",
                        item.path
                    ));
                    return;
                };
                if !target.is_file() {
                    self.hint(format!(
                        "本地没有这个文件：{}\n（没检出、已改名或已删除；回退只针对本地还在的文件）",
                        target.display()
                    ));
                    return;
                }
                (target, item.path.clone(), false)
            }
        };
        self.rollback = Some(Rollback {
            dir: index,
            revision: entry.revision.clone(),
            target,
            name,
            is_dir,
            confirm: false,
            error: String::new(),
            conflicts: blocked,
        });
        self.hint(format!("回退到 r{} 会改动本地文件，请在弹出窗口里确认", entry.revision));
    }

    /// Git 版的回退确认。整目录 = `git revert --no-commit {rev}..HEAD`，这是仓库级操作，
    /// 目录必须是仓库根；单文件 = `git restore --source={rev}`（文件在那次提交里还没有就删掉它）。
    /// 两种都只产生本地改动（含已暂存），不产生提交、不碰远端，确认窗口里讲清楚后果才执行。
    fn open_rollback_git(
        &mut self,
        index: usize,
        entry: &LogEntry,
        path: Option<&LogPath>,
        local: PathBuf,
        blocked: usize,
    ) {
        let Some(top) = self.git.toplevel(&local) else {
            self.hint("读不到 Git 仓库根目录，无法回退");
            return;
        };
        if top != local {
            self.hint("该目录不是 Git 仓库根：回退要按整个仓库来算，请把仓库根目录添加进来再回退");
            return;
        }
        let (target, name, is_dir) = match path {
            None => (local.clone(), self.dir_label(index), true),
            Some(item) => {
                // 日志里的路径相对仓库根，仓库根就是本目录时拼上就是本地真实文件
                let target = local.join(&item.path);
                if !target.is_file() {
                    self.hint(format!(
                        "本地没有这个文件：{}\n（没检出、已改名或已删除；回退只针对本地还在的文件）",
                        target.display()
                    ));
                    return;
                }
                (target, item.path.clone(), false)
            }
        };
        self.rollback = Some(Rollback {
            dir: index,
            revision: entry.revision.clone(),
            target,
            name,
            is_dir,
            confirm: false,
            error: String::new(),
            conflicts: blocked,
        });
        let rev = crate::repo::fmt_rev(self.dir_kind(index), &entry.revision);
        self.hint(format!("回退到 {rev} 会改动本地文件，请在弹出窗口里确认"));
    }

    /// 执行回退：`svn merge -r HEAD:<rev> --accept postpone <目标>`，产物是一批本地改动。
    /// 完成后重读 status 与检测，回退出来的那些项立刻出现在「待提交」里，由他决定上传还是撤销。
    pub fn spawn_rollback(
        &mut self,
        dir: usize,
        revision: String,
        target: PathBuf,
        name: String,
        is_dir: bool,
    ) {
        if self.dir_kind(dir) == RepoKind::Git {
            let Some(git) = self.git_or_none() else { return };
            if self.pool.has(Kind::Rollback, dir) {
                self.hint("上一次回退正在执行，请待其结束后再试");
                return;
            }
            let rev = crate::repo::fmt_rev(self.dir_kind(dir), &revision);
            self.pool.spawn(
                Kind::Rollback,
                dir,
                format!("{name} 回退 {rev}"),
                move |sink| {
                    let workdir = if is_dir {
                        target.clone()
                    } else {
                        target
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| target.clone())
                    };
                    if is_dir {
                        sink.line(format!(
                            "$ git revert --no-commit {revision}..HEAD \"{}\"",
                            target.display()
                        ));
                    } else {
                        sink.line(format!(
                            "$ git restore --staged --worktree --source={revision} \"{}\"",
                            target.display()
                        ));
                    }
                    let run = git.merge_back(
                        &workdir,
                        if is_dir { None } else { Some(target.as_path()) },
                        &revision,
                        &|line| sink.line(line),
                    );
                    let scope = if is_dir { "整个目录" } else { "该文件" };
                    let message = if run.ok {
                        format!(
                            "{name}：已回到 {rev}——{scope}的改动只落在本地，核对后再上传，不想要就「撤销修改」"
                        )
                    } else {
                        format!("{name}：回退到 {rev} 失败——{}", run.summary())
                    };
                    // 收尾文案只交给回收端统一写进输出区，避免同一句显示两遍
                    Data::Run {
                        dir,
                        ok: run.ok,
                        message,
                        reload: true,
                    }
                },
            );
            return;
        }
        let Some(svn) = self.svn_or_none() else {
            self.hint("没有可用的 svn.exe，无法回退");
            return;
        };
        if self.pool.has(Kind::Rollback, dir) {
            self.hint("上一次回退正在执行，请待其结束后再试");
            return;
        }
        self.pool.spawn(
            Kind::Rollback,
            dir,
            format!("{name} 回退 r{revision}"),
            move |sink| {
                let path = format!("\"{}\"", target.display());
                sink.line(format!(
                    "$ svn merge -r HEAD:{revision} --accept postpone {path} {path}"
                ));
                let run = svn.merge_back(&target, &revision, &|line| sink.line(line));
                let scope = if is_dir { "整个目录" } else { "该文件" };
                let summary = run.summary();
                let message = if run.ok {
                    format!(
                        "{name}：已回到 r{revision}——{scope}的改动只落在本地，核对后再上传，不想要就「撤销修改」"
                    )
                } else if summary.contains("E155035") {
                    // svn 只拒绝「回退要落在正冲突的那个节点」上，报错前一个文件都没动
                    format!(
                        "{name}：回退到 r{revision} 被冲突挡下——svn 不肯把回退落在正冲突的文件上，\
                         这一趟没有改动任何文件；先在目录行「更多 → 解决冲突」把它处理掉再回退：{summary}"
                    )
                } else {
                    format!("{name}：回退到 r{revision} 失败——{summary}")
                };
                // 收尾文案只交给回收端统一写进输出区，避免同一句显示两遍
                Data::Run {
                    dir,
                    ok: run.ok,
                    message,
                    reload: true,
                }
            },
        );
    }

    /// 打开「修改仓库地址」窗口：原前缀默认填仓库根地址，只需填新的根地址。
    pub fn open_relocate(&mut self, index: usize) {
        if self.dir_kind(index) == RepoKind::Git {
            self.hint("Git 仓库改远端地址请在终端执行 git remote set-url，这里不支持");
            return;
        }
        let Some(info) = self.dirs.get(index).and_then(|view| view.info.clone()) else {
            self.hint("还没有读到该目录的仓库地址，请先「全部检测」后再试");
            return;
        };
        let from = if info.repos_root.is_empty() {
            info.url.clone()
        } else {
            info.repos_root.clone()
        };
        self.relocate = Some(Relocate {
            dir: index,
            from,
            to: String::new(),
            focus: true,
            error: String::new(),
        });
    }

    /// 执行 `svn relocate`：改写工作副本记录的仓库地址，不更新、不改动任何本地文件。
    pub fn spawn_relocate(&mut self, index: usize, from: String, to: String, new_url: String) {
        let (Some(svn), Some(path)) = (self.svn_or_none(), self.dir_path(index)) else {
            return;
        };
        if self.pool.is_busy(index) {
            self.hint("该目录还有任务在执行，请稍后再试");
            return;
        }
        let label = self.dir_label(index);
        self.pool
            .spawn(Kind::Relocate, index, format!("{label} 修改仓库地址"), move |sink| {
                sink.line(format!(
                    "$ svn relocate \"{from}\" \"{to}\" \"{}\"",
                    path.display()
                ));
                let run = svn.relocate(&path, &from, &to, &|line| sink.line(line));
                let message = if run.ok {
                    format!("{label}：仓库地址已改为 {new_url}")
                } else {
                    format!("{label}：修改仓库地址失败——{}", run.summary())
                };
                // 收尾文案只交给任务回收端统一写进输出区，避免同一句显示两遍
                Data::Run {
                    dir: index,
                    ok: run.ok,
                    message,
                    reload: false,
                }
            });
    }

    // ------------------------------------------------------------ 目录管理

    pub fn add_directory(&mut self, raw: impl AsRef<str>) {
        let text = raw.as_ref().trim().trim_matches('"').to_owned();
        if text.is_empty() {
            self.hint("请先输入目录路径，或点击「选择文件夹」");
            return;
        }
        let path = PathBuf::from(&text);
        if !path.is_dir() {
            self.hint(format!("目录不存在：{text}"));
            return;
        }
        let clean = path.display().to_string();
        let exists = self.cfg.dirs.iter().any(|d| {
            d.path.replace('/', "\\").eq_ignore_ascii_case(&clean.replace('/', "\\"))
        });
        if exists {
            self.hint("该目录已在列表中");
            return;
        }
        let label = std::mem::take(&mut self.new_label);
        self.cfg
            .dirs
            .push(DirConfig { path: clean.clone(), label, ..Default::default() });
        self.sync_views();
        self.selected = Some(self.cfg.dirs.len() - 1);
        self.new_path.clear();
        self.persist();
        self.hint(format!("已添加目录：{clean}"));
        self.spawn_refresh(self.cfg.dirs.len() - 1, false);
    }

    pub fn remove_directory(&mut self, index: usize) {
        self.edit_label = None;
        self.edit_group = None;
        self.edit_tag = None;
        if index >= self.cfg.dirs.len() {
            return;
        }
        let removed = self.cfg.dirs.remove(index);
        self.dirs.remove(index);
        if self.relocate.as_ref().is_some_and(|d| d.dir >= index) {
            // 行号变了，窗口里的地址可能已经对不上，直接关掉重新打开
            self.relocate = None;
        }
        if self.commit_rules.as_ref().is_some_and(|d| d.dir >= index) {
            self.commit_rules = None;
        }
        self.sync_views();
        self.persist();
        self.confirm_remove = None;
        if self.commit.as_ref().is_some_and(|c| c.dir == index)
            || self.history.as_ref().is_some_and(|h| h.dir == index)
        {
            self.commit = None;
            self.history = None;
            self.page = Page::Main;
        }
        self.hint(format!("已从列表移除：{}（磁盘文件未删除）", removed.path));
    }

    pub fn move_directory(&mut self, index: usize, delta: isize) {
        // 上移 / 下移按「显示顺序」走：分组会把同名组的行拉到一起渲染，
        // 配置顺序 ≠ 看到的顺序，直接交换配置相邻行会出现「点了没反应」或跳位。
        // 在显示序列里找邻居，交换两者在配置里的位置，下一帧重算显示就是这两行互换
        let seq: Vec<usize> = self
            .group_segments()
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .collect();
        let Some(pos) = seq.iter().position(|i| *i == index) else {
            return;
        };
        let target_pos = pos as isize + delta;
        if target_pos < 0 || target_pos as usize >= seq.len() {
            return;
        }
        let from = seq[pos];
        let target = seq[target_pos as usize];
        self.cfg.dirs.swap(from, target);
        self.dirs.swap(from, target);
        if self.relocate.as_ref().is_some_and(|d| d.dir == from || d.dir == target) {
            self.relocate = None;
        }
        if self.commit_rules.as_ref().is_some_and(|d| d.dir == from || d.dir == target) {
            self.commit_rules = None;
        }
        self.selected = Some(target);
        self.edit_label = None;
        self.edit_group = None;
        self.edit_tag = None;
        self.persist();
    }

    /// 拖动落点用的搬运：把第 `from` 行挪到 `insert`（先摘掉 `from`、再数出来的位），
    /// 顺带把分组改写成 `group`（None = 分组不动）。
    /// 行序一变，各处指着 cfg.dirs 的下标都要跟着换算——视图状态、还开着的窗口、
    /// 批量窗口的勾选表，漏一处就会让提交页悄悄换成别的目录。
    pub fn shift_directory(&mut self, from: usize, insert: usize, group: Option<String>) {
        let len = self.cfg.dirs.len();
        if from >= len || insert >= len {
            return;
        }
        if from == insert {
            if let Some(group) = group {
                self.cfg.dirs[from].group = group;
                self.persist();
            }
            return;
        }
        // 旧下标 → 新下标：from 与 insert 之间那一段整体左移一位，反向那段整体右移一位
        let remap = |index: usize| -> usize {
            if index == from {
                insert
            } else if from < index && index < insert {
                index - 1
            } else if insert <= index && index < from {
                index + 1
            } else {
                index
            }
        };
        let mut moved = self.cfg.dirs.remove(from);
        if let Some(group) = group {
            moved.group = group;
        }
        self.cfg.dirs.insert(insert, moved);
        if from < self.dirs.len() {
            let view = self.dirs.remove(from);
            self.dirs.insert(insert.min(self.dirs.len()), view);
        }
        self.selected = Some(insert);
        self.edit_label = None;
        self.edit_group = None;
        self.edit_tag = None;
        if let Some(page) = self.commit.as_mut() {
            page.dir = remap(page.dir);
        }
        if let Some(page) = self.history.as_mut() {
            page.dir = remap(page.dir);
        }
        if let Some(dialog) = self.relocate.as_mut() {
            dialog.dir = remap(dialog.dir);
        }
        if let Some(dialog) = self.commit_rules.as_mut() {
            dialog.dir = remap(dialog.dir);
        }
        if let Some(dialog) = self.rollback.as_mut() {
            dialog.dir = remap(dialog.dir);
        }
        for (dir, _) in &mut self.pending_refresh {
            *dir = remap(*dir);
        }
        // 勾选表与 cfg.dirs 一行一位，那一行搬走，勾也要一起搬
        let shift_flags = |flags: &mut Vec<bool>| {
            if from < flags.len() {
                let checked = flags.remove(from);
                flags.insert(insert.min(flags.len()), checked);
            }
        };
        if let Some(window) = self.upload_all.as_mut() {
            shift_flags(&mut window.checked);
        }
        if let Some(window) = self.batch_relocate.as_mut() {
            shift_flags(&mut window.checked);
        }
        if let Some(window) = self.batch_label.as_mut() {
            shift_flags(&mut window.checked);
        }
        self.persist();
    }

    pub fn open_folder(&self, index: usize) {
        let Some(path) = self.dir_path(index) else {
            return;
        };
        let _ = std::process::Command::new("explorer").arg(&path).spawn();
    }

    /// 重置 Beyond Compare 的试用状态：删掉注册表里的 CacheID，结果逐条写进输出记录。

    pub fn open_commit(&mut self, index: usize) {
        if self.dir_kind(index) == RepoKind::Git {
            if self.git_or_none().is_none() {
                return;
            }
        } else if self.svn_or_none().is_none() {
            return;
        }
        let label = self.dir_label(index);
        self.commit = Some(CommitPage::new(index, label));
        self.page = Page::Commit;
        self.spawn_status(index, false);
    }

    pub fn open_history(&mut self, index: usize) {
        if self.dir_kind(index) == RepoKind::Git {
            if self.git_or_none().is_none() {
                return;
            }
        } else if self.svn_or_none().is_none() {
            return;
        }
        let label = self.dir_label(index);
        let limit = self.cfg.log_limit;
        // git 没有 svn auth 探测：提交人取仓库配置的 user.name，取不到就留空（列表不筛人）
        let user = if self.dir_kind(index) == RepoKind::Git {
            self.dir_path(index)
                .and_then(|path| self.git.auth_user(&path))
                .unwrap_or_default()
        } else {
            self.svn_user.clone()
        };
        let only_mine = self.cfg.history_only_mine;
        let page = HistoryPage::new(index, label, limit, user, only_mine);
        self.history = Some(page);
        self.page = Page::History;
        let mine = self.history.as_ref().is_some_and(|page| page.mine);
        self.spawn_log_in(index, mine, false, None);
    }

    pub fn back_to_main(&mut self) {
        self.page = Page::Main;
    }
}

/// 合并落盘与还原共用的收尾消息。后台闭包拿不到 `self`，所以是模块级函数而不是方法：
/// 项数与备份位置说明「动了什么」，没处理成功的那几项单独点名，零项时给正面说法
fn merge_write_result(
    index: usize,
    label: &str,
    verb: &str,
    result: Result<(usize, u64, Vec<String>, Option<String>), String>,
    sink: &crate::jobs::Sink,
) -> Data {
    let (message, ok) = match result {
        Ok((count, bytes, failed, backup)) => {
            if count == 0 && failed.is_empty() {
                (format!("{label}：两边已经一致，{verb} 0 项"), true)
            } else {
                let size =
                    if bytes > 0 { format!("，共 {}", crate::merge::human_bytes(bytes)) } else { String::new() };
                // 直接合并那趟没有还原点，尾巴得说明白，别让人以为还能一键退回
                let where_back = match backup {
                    Some(dir) => format!("，备份在 {dir}"),
                    None => "，本次未备份".to_owned(),
                };
                let mut text = format!("{label}：已{verb} {count} 项{size}{where_back}");
                if !failed.is_empty() {
                    text.push_str(&format!("；{} 项没成功：{}", failed.len(), failed.join("；")));
                }
                (text, failed.is_empty())
            }
        }
        Err(reason) => (format!("{label}：没有落盘——{reason}"), false),
    };
    sink.line(format!("→ {message}"));
    // 没落盘时不补检测：树没变，重跑一遍只是白等
    Data::Run { dir: index, ok, message, reload: ok }
}
