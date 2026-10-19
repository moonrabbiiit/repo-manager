//! 双向合并的算法层：把「本地开发侧」与「服务器端工作副本」相对同一个祖先各自的改动
//! 拼到对方身上，产出两份按 Beyond Compare 等价口径内容相同、却各自保留自己 BOM 与行尾的字节。
//!
//! 分层：行级算法（`three_way` 那一组）是纯计算，不走树、不碰 svn、不写文件；`plan` 读两边
//! 字节并问祖先（祖先查询走 `Ancestor` trait，svn.exe 的实现放在 `svn.rs`，测试喂内存桩）；
//! 项目文件只有 `apply` 修改；预览仅在配置目录保存已一致文件的同步快照。
//!
//! 为什么不直接拿 `svn cat -r BASE` 当祖先：工作副本 update 过之后 BASE 已是新版本，
//! 而本地开发侧还停在老代码上。此时祖先=BASE 会把「本地没跟着更新」算成「本地把新版本
//! 退回了」，双向合并的结果是把别人的提交反向覆盖掉。所以祖先必须是「两边上次一致的那个
//! 状态」；定不了岔就整文件跳过，不猜。
//!
//! 为什么按字节切行而不解码：这个仓库里 UTF-8 与 GBK 源码并存。GBK 的尾字节落在
//! 0x40–0xFE，取不到 0x0A 也取不到 0x0D，所以按 0x0A 切行、按 0x0D 认行尾，两种编码的中文
//! 注释都不会被切坏；反过来先解码就得引入一套编码嗅探，猜错了才是真坏。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::localdiff::{looks_text, normalized_lines, read_limited, Excludes, Limits, Pair, NORMALIZE_MAX, Verdict};

pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TEXT_VIEW_BYTES: usize = 64 * 1024 * 1024;

fn read_bytes(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> { read_limited(path.as_ref(), MAX_FILE_BYTES) }

/// LCS 的规模上限（两侧行数之积）。超过就判「太大不算」，不去跑一个几十亿格的动态规划——
/// 合并要的是「不确定就别动」，不是「再慢也要算出来」。
pub const LCS_CELL_LIMIT: usize = 60_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Cr,
    CrLf,
    /// 文件末尾没有换行的那一行
    Eof,
}

impl Eol {
    fn bytes(self) -> &'static [u8] {
        match self {
            Eol::Lf => b"\n",
            Eol::Cr => b"\r",
            Eol::CrLf => b"\r\n",
            Eol::Eof => b"",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Line<'a> {
    pub body: &'a [u8],
    pub eol: Eol,
}

/// 一个文本文件的行视图。BOM 与行尾各自记账，`render(parse(x)) == x` 恒等——
/// 合并产物只有在该侧原本的字节上插入另一侧的段落，绝不顺手改写行尾。
#[derive(Clone, Debug)]
pub struct FileText<'a> {
    pub bom: bool,
    pub lines: Vec<Line<'a>>,
}

impl<'a> FileText<'a> {
    pub fn parse(raw: &'a [u8]) -> Self {
        let (bom, body) = match raw.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        let mut lines = Vec::new();
        let mut start = 0;
        let mut i = 0;
        while i < body.len() {
            match body[i] {
                b'\n' => {
                    lines.push(Line { body: &body[start..i], eol: Eol::Lf });
                    i += 1;
                    start = i;
                }
                b'\r' => {
                    let (eol, step) = if body.get(i + 1) == Some(&b'\n') { (Eol::CrLf, 2) } else { (Eol::Cr, 1) };
                    lines.push(Line { body: &body[start..i], eol });
                    i += step;
                    start = i;
                }
                _ => i += 1,
            }
        }
        if start < body.len() {
            lines.push(Line { body: &body[start..], eol: Eol::Eof });
        }
        Self { bom, lines }
    }

    /// 整份还原成字节：落盘不从这里走（合并结果在 `three_way_limited` 里直接拼），只有测试拿它验切分与回写一致
    #[cfg(test)]
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.bom {
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        emit_lines(&mut out, &self.lines);
        out
    }

    /// 比较用的行键序列：行尾空白不参与（与 `localdiff::normalize` 同一条规则），
    /// BOM 与行尾已经在 `parse` 里单独记账，所以也不在这里。
    pub fn keys(&self) -> Vec<&[u8]> {
        self.lines.iter().map(|l| line_key(l.body)).collect()
    }
}

fn emit_lines(out: &mut Vec<u8>, lines: &[Line]) {
    for (i, line) in lines.iter().enumerate() {
        out.extend_from_slice(line.body);
        match line.eol {
            // 中间位置遇到「原本没有换行的末行」：要接上下面的内容，只能补一个换行
            Eol::Eof if i + 1 < lines.len() => out.push(b'\n'),
            eol => out.extend_from_slice(eol.bytes()),
        }
    }
}

/// 去掉行尾的空格、制表符与 0x0B/0x0C，与 `localdiff::normalize` 对每行做的事一致
fn line_key(body: &[u8]) -> &[u8] {
    let end = match body.iter().rposition(|b| !matches!(b, b' ' | b'\t' | 0x0B | 0x0C)) {
        Some(kept) => kept + 1,
        None => 0,
    };
    &body[..end]
}

/// 两份字节按 BC 等价口径是否同一内容：测试里的总不变量
pub fn content_equal(a: &[u8], b: &[u8]) -> bool {
    a == b || (a.len().max(b.len()) as u64 <= NORMALIZE_MAX
        && looks_text(a) && looks_text(b)
        && normalized_lines(a).eq(normalized_lines(b)))
}

/// 归一化行多重集的对称差，给定岔时量「某个历史版本离本地侧有多远」。
/// 不按位置比：上游在开头插了三行而本地没动，答案是 3，而不是整篇错位后的天文数字。
pub fn bag_distance(a: &[u8], b: &[u8]) -> usize {
    let count = normalized_lines(a).count().saturating_add(normalized_lines(b).count());
    // 距离只用于诊断，不为大文件建立无界哈希表。
    if count > 100_000 { return count }
    let mut bag: HashMap<&[u8], i32> = HashMap::new();
    for key in normalized_lines(a) {
        *bag.entry(key).or_default() += 1;
    }
    for key in normalized_lines(b) {
        *bag.entry(key).or_default() -= 1;
    }
    bag.values().map(|n| n.unsigned_abs() as usize).sum()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Keep { a: usize, b: usize, len: usize },
    Delete { a: usize, len: usize },
    Insert { b: usize, len: usize },
}

/// 两组行键之间的编辑序列（精确 LCS，Hirschberg 线性空间）。不引第三方 diff crate。
pub fn lcs_ops(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    ops_from_pairs(&lcs_pairs(a, b), a.len(), b.len())
}

fn lcs_pairs(a: &[&[u8]], b: &[&[u8]]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    hirschberg(a, b, 0, 0, &mut out);
    out
}

fn hirschberg(a: &[&[u8]], b: &[&[u8]], base_a: usize, base_b: usize, out: &mut Vec<(usize, usize)>) {
    if a.is_empty() || b.is_empty() {
        return;
    }
    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < a.len() - head && tail < b.len() - head && a[a.len() - 1 - tail] == b[b.len() - 1 - tail] {
        tail += 1;
    }
    for i in 0..head {
        out.push((base_a + i, base_b + i));
    }
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let (na, nb) = (base_a + head, base_b + head);
    if !am.is_empty() && !bm.is_empty() {
        if am.len() == 1 {
            // 只剩一行：LCS 长度非 0 即 1，配上第一个相等项就到底了
            if let Some(j) = bm.iter().position(|x| *x == am[0]) {
                out.push((na, nb + j));
            }
        } else {
            let mid = am.len() / 2;
            let left = lcs_row_head(&am[..mid], bm);
            let right = lcs_row_tail(&am[mid..], bm);
            let mut split = 0;
            let mut best = 0u32;
            for x in 0..=bm.len() {
                let total = left[x] + right[x];
                if total > best {
                    best = total;
                    split = x;
                }
            }
            hirschberg(&am[..mid], &bm[..split], na, nb, out);
            hirschberg(&am[mid..], &bm[split..], na + mid, nb + split, out);
        }
    }
    for i in 0..tail {
        out.push((base_a + a.len() - tail + i, base_b + b.len() - tail + i));
    }
}

/// `out[x] == LCS(a, b[..x])`
fn lcs_row_head(a: &[&[u8]], b: &[&[u8]]) -> Vec<u32> {
    let mut prev = vec![0u32; b.len() + 1];
    let mut cur = vec![0u32; b.len() + 1];
    for i in 0..a.len() {
        cur[0] = 0;
        for j in 1..=b.len() {
            cur[j] = if a[i] == b[j - 1] { prev[j - 1] + 1 } else { prev[j].max(cur[j - 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev
}

/// `out[x] == LCS(a, b[x..])`
fn lcs_row_tail(a: &[&[u8]], b: &[&[u8]]) -> Vec<u32> {
    let mut next = vec![0u32; b.len() + 1];
    let mut cur = vec![0u32; b.len() + 1];
    for i in (0..a.len()).rev() {
        cur[b.len()] = 0;
        for j in (0..b.len()).rev() {
            cur[j] = if a[i] == b[j] { next[j + 1] + 1 } else { next[j].max(cur[j + 1]) };
        }
        std::mem::swap(&mut next, &mut cur);
    }
    next
}

fn ops_from_pairs(pairs: &[(usize, usize)], a_len: usize, b_len: usize) -> Vec<Op> {
    let mut ops = Vec::new();
    let (mut a, mut b) = (0usize, 0usize);
    let mut i = 0;
    while i < pairs.len() {
        let (pa, pb) = pairs[i];
        if pa > a {
            ops.push(Op::Delete { a, len: pa - a });
        }
        if pb > b {
            ops.push(Op::Insert { b, len: pb - b });
        }
        let mut len = 1;
        while i + len < pairs.len() && pairs[i + len].0 == pa + len && pairs[i + len].1 == pb + len {
            len += 1;
        }
        ops.push(Op::Keep { a: pa, b: pb, len });
        a = pa + len;
        b = pb + len;
        i += len;
    }
    if a < a_len {
        ops.push(Op::Delete { a, len: a_len - a });
    }
    if b < b_len {
        ops.push(Op::Insert { b, len: b_len - b });
    }
    ops
}

/// 一侧相对祖先的一处改动：祖先行区间 `anc..anc+len` 换成 `side..side+side_len`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Change {
    anc: usize,
    len: usize,
    side: usize,
    side_len: usize,
}

fn changes_from_ops(ops: &[Op]) -> Vec<Change> {
    let mut out = Vec::new();
    let (mut ac, mut sc) = (0usize, 0usize);
    for op in ops {
        match op {
            Op::Keep { len, .. } => {
                ac += len;
                sc += len;
            }
            Op::Delete { len, .. } => {
                out.push(Change { anc: ac, len: *len, side: sc, side_len: 0 });
                ac += len;
            }
            Op::Insert { len, .. } => {
                // 紧接在一段删除之后的插入是同一处替换：祖先那段换成这一段的新区块
                let merged = match out.last_mut() {
                    Some(prev) if prev.side_len == 0 && prev.anc + prev.len == ac && prev.side == sc => {
                        prev.side_len = *len;
                        true
                    }
                    _ => false,
                };
                if !merged {
                    out.push(Change { anc: ac, len: 0, side: sc, side_len: *len });
                }
                sc += len;
            }
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Region {
    /// 祖先行区间 `[lo, hi)`；纯插入时两端相同
    pub anc: (usize, usize),
    /// 改过的一侧在该文件里的行区间；没改的那一侧是 `None`
    pub local: Option<(usize, usize)>,
    pub server: Option<(usize, usize)>,
    pub kind: RegionKind,
}

#[derive(Clone, Debug, Default)]
pub struct Merge3 {
    pub regions: Vec<Region>,
    /// `None` = 有冲突或太大，整文件不动
    pub merged_local: Option<Vec<u8>>,
    pub merged_server: Option<Vec<u8>>,
    pub too_big: bool,
}

impl Merge3 {
    pub fn has_conflict(&self) -> bool {
        self.regions.iter().any(|r| r.kind == RegionKind::Conflict)
    }

    #[cfg(test)]
    pub fn changed(&self) -> bool {
        !self.regions.is_empty()
    }
}

#[cfg(test)]
pub fn three_way(anc: &[u8], local: &[u8], server: &[u8]) -> Merge3 {
    three_way_limited(anc, local, server, LCS_CELL_LIMIT)
}

/// `cell_limit` 是给测试留的口子：真实调用一律走 `LCS_CELL_LIMIT`
pub fn three_way_limited(anc: &[u8], local: &[u8], server: &[u8], cell_limit: usize) -> Merge3 {
    let counts = [anc, local, server].map(|bytes| normalized_lines(bytes).count());
    if counts[0].saturating_mul(counts[1]) > cell_limit || counts[0].saturating_mul(counts[2]) > cell_limit
        || counts.iter().sum::<usize>().saturating_mul(96) > MAX_TEXT_VIEW_BYTES
    { return Merge3 { too_big: true, ..Default::default() } }
    let (fa, fl, fs) = (FileText::parse(anc), FileText::parse(local), FileText::parse(server));
    let (ka, kl, ks) = (fa.keys(), fl.keys(), fs.keys());
    if ka.len().saturating_mul(kl.len()) > cell_limit || ka.len().saturating_mul(ks.len()) > cell_limit {
        return Merge3 { too_big: true, ..Default::default() };
    }
    let regions = cluster(
        &changes_from_ops(&lcs_ops(&ka, &kl)),
        &changes_from_ops(&lcs_ops(&ka, &ks)),
        &kl,
        &ks,
    );
    if regions.iter().any(|r| r.kind == RegionKind::Conflict) {
        return Merge3 { regions, ..Default::default() };
    }
    Merge3 {
        merged_local: render_merged(&fl, &fs, &regions, true),
        merged_server: render_merged(&fs, &fl, &regions, false),
        regions,
        too_big: false,
    }
}

/// 把两边的改动按祖先行区间聚成区域：不相交的各归各，相交了才需要判是同一次改动还是冲突
fn cluster(local: &[Change], server: &[Change], kl: &[&[u8]], ks: &[&[u8]]) -> Vec<Region> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < local.len() || j < server.len() {
        let seed = next_change(local, server, i, j);
        let (mut lo, mut hi) = (seed.anc, seed.anc + seed.len);
        let (mut l_side, mut l_anc) = (None, None);
        let (mut s_side, mut s_anc) = (None, None);
        // 吸进一条就可能把组的祖先行区间拉长，拉长后又可能吸进更后面那条：反复到不再有新增
        loop {
            let mut grew = false;
            while i < local.len() && overlaps(lo, hi, &local[i]) {
                let c = local[i];
                l_side = Some(union_range(l_side, (c.side, c.side + c.side_len)));
                l_anc = Some(union_range(l_anc, (c.anc, c.anc + c.len)));
                (lo, hi) = union_range(Some((lo, hi)), (c.anc, c.anc + c.len));
                i += 1;
                grew = true;
            }
            while j < server.len() && overlaps(lo, hi, &server[j]) {
                let c = server[j];
                s_side = Some(union_range(s_side, (c.side, c.side + c.side_len)));
                s_anc = Some(union_range(s_anc, (c.anc, c.anc + c.len)));
                (lo, hi) = union_range(Some((lo, hi)), (c.anc, c.anc + c.len));
                j += 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }
        let kind = match (l_side, s_side) {
            (Some(_), None) => RegionKind::LocalOnly,
            (None, Some(_)) => RegionKind::ServerOnly,
            (Some(ls), Some(ss)) => {
                // 两侧都动了同一处，还得是「同一次改动」才算合过：覆盖的祖先行区间必须彼此
                // 相等并等于整组，否则一边删 3..5、另一边删 4..6 这种也会被当成相同改动
                let same_span = l_anc == s_anc && l_anc == Some((lo, hi));
                if same_span && kl[ls.0..ls.1] == ks[ss.0..ss.1] {
                    RegionKind::BothSame
                } else {
                    RegionKind::Conflict
                }
            }
            // 不可达：seed 一定落在本组里，至少有一侧有改动
            (None, None) => RegionKind::Conflict,
        };
        out.push(Region { anc: (lo, hi), local: l_side, server: s_side, kind });
    }
    out
}

fn union_range(cur: Option<(usize, usize)>, next: (usize, usize)) -> (usize, usize) {
    match cur {
        None => next,
        Some((lo, hi)) => (lo.min(next.0), hi.max(next.1)),
    }
}

fn next_change(local: &[Change], server: &[Change], i: usize, j: usize) -> Change {
    match (local.get(i), server.get(j)) {
        (Some(a), Some(b)) if a.anc <= b.anc => *a,
        (Some(_), Some(b)) => *b,
        (Some(a), None) => *a,
        (None, Some(b)) => *b,
        (None, None) => unreachable!("cluster 只在至少一侧还剩改动时才取 seed"),
    }
}

/// 祖先行区间是否算「动了同一处」。区间相交才算；两处纯插入落在同一个插入点也算
/// （顺序无从判断），贴着区间的端点插入不算。
fn overlaps(lo: usize, hi: usize, c: &Change) -> bool {
    if c.len == 0 && hi == lo {
        return c.anc == lo;
    }
    c.anc < hi && lo < c.anc + c.len
}

/// 本侧通篇用哪种行尾：第一条带换行的行说了算；整份文件没有换行（空文件、单行无尾）时按 LF。
/// 从另一侧拉进来的段落要改写成这个行尾，否则把服务器的 CRLF 块塞进本地的 LF 文件里，
/// 合并产物就成了混合行尾的文件——BC 认它等价，编辑器与 `svn diff` 不认。
fn dominant_eol(text: &FileText) -> Eol {
    text.lines.iter().find(|l| l.eol != Eol::Eof).map(|l| l.eol).unwrap_or(Eol::Lf)
}

fn render_merged(target: &FileText, other: &FileText, regions: &[Region], target_is_local: bool) -> Option<Vec<u8>> {
    let mut picked: Vec<Line> = Vec::new();
    let style = dominant_eol(target);
    let (mut ac, mut tc) = (0usize, 0usize);
    for region in regions {
        if region.kind == RegionKind::Conflict {
            return None;
        }
        let own = if target_is_local { region.local } else { region.server };
        let theirs = if target_is_local { region.server } else { region.local };
        // 上一组之后到这一组之间的原文：两侧都是「祖先的这一段」，行号一一对应
        let gap = region.anc.0 - ac;
        picked.extend_from_slice(&target.lines[tc..tc + gap]);
        tc += gap;
        match own {
            // 目标侧在这组里有自己的改动：按它的行区间走，行数差正是它插入/删除出来的
            Some((from, to)) => {
                picked.extend_from_slice(&target.lines[from..to]);
                tc = to;
            }
            None => {
                if let Some((from, to)) = theirs {
                    picked.extend(other.lines[from..to].iter().map(|l| Line { body: l.body, eol: style }));
                }
                tc += region.anc.1 - region.anc.0;
            }
        }
        ac = region.anc.1;
    }
    picked.extend_from_slice(&target.lines[tc..]);
    let mut out = Vec::new();
    if target.bom {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    emit_lines(&mut out, &picked);
    Some(out)
}

/// 白名单：「参与合并的文件」那一格先过一遍与屏蔽框同一套缝合规则，再解析成匹配器。
/// 留空就换成内置的代码 / 文本清单（老配置不用迁移也能有个合理范围）；写了内容就只按写的走。
/// 判定用 `Excludes::hits`，包含模式下它的含义是反的：命中自身或任一级父目录 = 参与合并。
pub(crate) fn scope(raw: &str) -> Excludes {
    let rules = crate::commit::normalize_includes(raw);
    let rules = if rules.trim().is_empty() { crate::config::DEFAULT_MERGE_INCLUDE } else { &rules };
    Excludes::parse_includes(rules)
}

/// 黑名单：「不参与合并的文件」那一格，缝合规则与匹配器都跟「屏蔽可提交项」共用一份，
/// 只是在白名单之后再判一次——两格同时命中时以这一格为准，「明确写了不要」比「顺手加了进来」更硬。
/// 留空返回 `None`，一条都不额外挡
pub(crate) fn blacklist(raw: &str) -> Option<Excludes> {
    let rules = crate::commit::normalize_excludes(raw);
    (!rules.trim().is_empty()).then(|| Excludes::parse(&rules))
}

// ---------------------------------------------------------------------- 祖先

/// 服务器端那个文件在工作副本里的状态。合并前先问一次：「拷回去」对计划删除的文件
/// 等于把这次删除撤销掉，对未解决冲突的文件等于替人做决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
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
#[derive(Default)]
struct Budget {
    calls: usize,
    offline: String,
}

struct AncestorOf {
    bytes: Vec<u8>,
    info: AncInfo,
}

/// 祖先推导。工作副本 update 过之后 BASE 已是新版本、本地侧还停在老代码上，此时拿 BASE
/// 当祖先会把「本地没跟着更新」算成「本地把新版本退回了」，双向合并的结果是把别人的提交
/// 反向覆盖掉。因此只接受可验证的版本匹配；历史内容相似只能作诊断，不能决定祖先。
fn ancestor_of(
    anc: &dyn Ancestor,
    local_anc: &dyn Ancestor,
    rel: &str,
    local: &[u8],
    budget: &mut Budget,
    limits: &MergeLimits,
) -> AncestorOf {
    let unknown = |bytes: Vec<u8>, reason: Uncertain| AncestorOf { bytes, info: AncInfo::Unknown(reason) };
    budget.calls += 1;
    let base = match anc.base(rel) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return unknown(Vec::new(), Uncertain::NoBase("该文件没有 BASE 版本".to_owned())),
        Err(msg) => return unknown(Vec::new(), Uncertain::NoBase(msg)),
    };
    let base_distance = bag_distance(&base, local);
    if content_equal(&base, local) {
        return AncestorOf { bytes: base, info: AncInfo::Base };
    }
    // 本地也受 SVN 管理时，用它的 BASE 提供版本证据。相似度只作诊断，不能授权覆盖。
    let local_base = if local_anc.wc_state(rel, true) == WcState::Normal {
        budget.calls += 1;
        match local_anc.base(rel) {
            Ok(bytes) => bytes,
            Err(msg) => return unknown(base, Uncertain::NoBase(msg)),
        }
    } else { None };
    if local_base.as_deref().is_some_and(|bytes| content_equal(bytes, &base)) {
        return AncestorOf { bytes: base, info: AncInfo::Base };
    }
    budget.calls += 1;
    let history = match anc.revs(rel, limits.history_max) {
        Ok(revs) => revs,
        // 读记录本身就失败，一个候选都试不到；记成断线，别让报告说成「窗口内没收拢」
        Err(msg) => {
            budget.offline = msg;
            Vec::new()
        }
    };
    let mut best: Option<(u64, usize, Vec<u8>)> = None;
    for rev in history {
        budget.calls += 1;
        let bytes = match anc.at_rev(rel, rev) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(msg) => {
                if msg.contains("读取上限") { return unknown(base, Uncertain::NoBase(msg)); }
                budget.offline = msg;
                break;
            }
        };
        let distance = bag_distance(&bytes, local);
        let expected = local_base.as_deref().unwrap_or(local);
        if content_equal(&bytes, expected) {
            return AncestorOf { bytes, info: AncInfo::Rev(rev) };
        }
        if best.as_ref().map(|(_, d, _)| distance < *d).unwrap_or(true) {
            best = Some((rev, distance, bytes));
        }
    }
    // 没有准确匹配的版本证据就交人工，不以“差得不远”作为共同祖先。
    let reason = if !budget.offline.is_empty() {
        Uncertain::Offline(budget.offline.clone())
    } else if best.as_ref().is_some_and(|(_, distance, _)| *distance <= limits.give_up_lines)
        || base_distance <= limits.give_up_lines
    {
        Uncertain::Unproven
    } else if let Some((rev, distance, _)) = best.as_ref() {
        Uncertain::TooFar { candidate: *rev, distance: *distance, threshold: limits.give_up_lines }
    } else {
        Uncertain::WindowClosed { distance: base_distance, window: limits.history_max }
    };
    unknown(base, reason)
}

// ---------------------------------------------------------------------- 计划

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
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
    #[serde(skip)]
    pub write_local: Option<Vec<u8>>,
    #[serde(skip)]
    pub write_server: Option<Vec<u8>>,
    /// 服务器端已删除、要同步删掉本地这份（与 `write_local` 互斥：删和写不并存）
    pub delete_local: bool,
    pub delete_server: bool,
    /// 预览实际读取的两侧字节；缺失也必须在应用时重新核对。
    #[serde(skip)]
    pub observed_local: Option<Vec<u8>>,
    #[serde(skip)]
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
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Plan {
    pub entries: Vec<Entry>,
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
    #[serde(skip)]
    pub validation_client: Option<crate::svn::Svn>,
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

#[derive(Default, Serialize, Deserialize)]
struct SyncState {
    version: u32,
    local_root: String,
    server_root: String,
    revision: u64,
    /// 不把字节塞进 JSON：源码可用 GBK，二进制文件也要保留原字节。
    files: HashMap<String, String>,
    /// 只有完整扫过相同范围，才可证明此前不存在的路径是本次新增。
    #[serde(default)]
    complete_scope: Option<String>,
    #[serde(default)]
    inventory: std::collections::HashSet<String>,
    #[serde(default)]
    new_files: std::collections::HashSet<String>,
    #[serde(default)]
    bindings: Option<(Option<RepositoryBinding>, Option<RepositoryBinding>)>,
    #[serde(default)]
    backup_root: Option<PathBuf>,
    #[serde(default)]
    recovery_seen: std::collections::HashSet<String>,
    #[serde(default)]
    backup_sessions: std::collections::HashSet<String>,
    #[serde(default)]
    metadata: Option<(Option<Metadata>, Option<Metadata>)>,
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

/// Windows share_mode(0) 随句柄关闭自动释放；进程崩溃不会留下无法重跑的锁。
fn lock_state(path: &Path) -> Result<File, String> {
    let parent = path.parent().ok_or("同步状态路径没有父目录")?;
    fs::create_dir_all(parent).map_err(|e| format!("创建同步状态目录失败：{e}"))?;
    let mut options = fs::OpenOptions::new();
    let lock = parent.join("state.lock");
    if !lock.exists() {
        match fs::OpenOptions::new().create_new(true).write(true).open(&lock) {
            Ok(file) => drop(file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(e) => return Err(format!("创建同步状态锁失败：{e}")),
        }
    }
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options.open(lock).map_err(|e| format!("同步状态正被使用或无法读取，请稍后重试：{e}"))
}

impl SyncState {
    fn load(path: &Path, local: &Path, server: &Path) -> Result<Self, String> {
        let raw = match read_bytes(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self {
                version: 1, local_root: root_key(local), server_root: root_key(server), ..Default::default()
            }),
            Err(e) => return Err(format!("读取同步基线失败：{e}")),
        };
        let mut state: Self = serde_json::from_slice(&raw).map_err(|e| format!("同步基线已损坏：{e}"))?;
        if state.version != 1 || state.local_root != root_key(local) || state.server_root != root_key(server) {
            return Err("同步基线版本或绑定目录不匹配，请先人工核对".into());
        }
        if state.backup_root.is_none() {
            if let Some(parent) = path.parent().and_then(Path::parent) {
                if parent.file_name().is_some_and(|name| name == "merge-state") {
                    state.backup_root = parent.parent().map(Path::to_path_buf);
                }
            }
        }
        if let Some(root) = &state.backup_root {
            let entries = match fs::read_dir(root) {
                Ok(entries) => Some(entries),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("无法核对备份恢复记录：{e}")),
            };
            if let Some(entries) = entries {
                for item in entries {
                    let item = item.map_err(|e| format!("读取备份恢复记录失败：{e}"))?;
                    let id = item.file_name().to_string_lossy().into_owned();
                    if !id.starts_with(BACKUP_PREFIX) || state.recovery_seen.contains(&id)
                        || (!state.backup_sessions.is_empty() && !state.backup_sessions.contains(&id)) { continue }
                    let lines = read_journal(&item.path())?;
                    if lines.iter().any(|l| l.kind == "restore_begin" && l.state_path.as_deref() == Some(path)) {
                        for line in lines.iter().filter(|l| l.kind == "begin") {
                            state.files.remove(&line.rel);
                            state.new_files.remove(&line.rel);
                        }
                        state.recovery_seen.insert(id);
                    }
                }
            }
        }
        Ok(state)
    }

    fn baseline(&self, path: &Path, key: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(name) = self.files.get(key) else { return Ok(None) };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err("同步快照路径无效".into());
        }
        read_bytes(path.parent().unwrap().join("snapshots").join(name))
            .map(Some).map_err(|e| format!("读取 {key} 的同步快照失败：{e}"))
    }

    fn remember(&mut self, path: &Path, key: &str, bytes: &[u8]) -> Result<(), String> {
        self.new_files.remove(key);
        if self.baseline(path, key)?.as_deref() == Some(bytes) { return Ok(()) }
        let name = format!("{}-{}-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id(), TEMP_SEQ.fetch_add(1, Ordering::Relaxed));
        replace(&path.parent().unwrap().join("snapshots").join(&name), bytes)?;
        self.files.insert(key.to_owned(), name);
        Ok(())
    }

    fn save(&mut self, path: &Path) -> Result<(), String> {
        self.revision = self.revision.checked_add(1).ok_or("同步状态版本号溢出")?;
        let raw = serde_json::to_vec(self).map_err(|e| format!("序列化同步状态失败：{e}"))?;
        replace(path, &raw)?;
        // 已提交的新清单是唯一引用源；锁仍持有时清掉旧快照和中断遗留，避免长期堆积源码副本。
        if let Ok(entries) = fs::read_dir(path.parent().unwrap().join("snapshots")) {
            let referenced: std::collections::HashSet<&str> = self.files.values().map(String::as_str).collect();
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_file())
                    && !referenced.contains(entry.file_name().to_string_lossy().as_ref())
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }
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

    /// 原文备份与用于安全撤销的写入产物快照的总字节数。
    pub fn backup_bytes(&self) -> u64 {
        pending(self)
            .into_iter()
            .flat_map(|(_, jobs)| jobs)
            .map(|j| j.before.map(|f| f.size).unwrap_or_default() + j.bytes.len() as u64)
            .sum()
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

/// 一条差异参不参与合并：白名单先放行，黑名单再收回。两格同时命中时按黑名单算
struct Scope {
    allow: Excludes,
    block: Option<Excludes>,
}

impl Scope {
    fn new(include: &str, exclude: &str) -> Self {
        Self { allow: scope(include), block: blacklist(exclude) }
    }

    /// 不参与时该记哪一种动作；参与则返回 `None`
    fn skipped_as(&self, key: &str) -> Option<Action> {
        if self.block.as_ref().is_some_and(|b| b.hits(key)) {
            return Some(Action::SkipBlacklisted);
        }
        (!self.allow.hits(key)).then_some(Action::SkipNotWhitelisted)
    }
}

/// 扫一遍差异，逐项定出动作。永远不写文件
#[cfg(test)]
pub fn plan(
    local_root: &Path,
    server_root: &Path,
    filter: &str,
    include: &str,
    exclude: &str,
    anc: &dyn Ancestor,
    local_anc: &dyn Ancestor,
    limits: &MergeLimits,
) -> Result<Plan, String> {
    plan_with_state(local_root, server_root, filter, include, exclude, anc, local_anc, limits, None)
}

pub fn plan_with_state(
    local_root: &Path, server_root: &Path, filter: &str, include: &str, exclude: &str,
    anc: &dyn Ancestor, local_anc: &dyn Ancestor, limits: &MergeLimits, state_path: Option<&Path>,
) -> Result<Plan, String> {
    let _state_lock = state_path.map(lock_state).transpose()?;
    let mut state = state_path.map(|path| SyncState::load(path, local_root, server_root)).transpose()?;
    let server_binding = anc.binding()?;
    let local_binding = local_anc.binding()?;
    let metadata = (local_anc.metadata()?, anc.metadata()?);
    let bindings = (local_binding.clone(), server_binding.clone());
    let mut binding_reset = false;
    if let Some(state) = state.as_mut() {
        if state.bindings.as_ref() != Some(&bindings) {
            binding_reset = !state.files.is_empty() || !state.inventory.is_empty();
            state.files.clear();
            state.inventory.clear();
            state.new_files.clear();
            state.complete_scope = None;
            state.bindings = Some(bindings);
            state.metadata = None;
        }
    }
    let scan = crate::localdiff::pairs_for_merge(
        local_root,
        server_root,
        filter,
        &Limits { max_files: limits.max_files, max_time: limits.max_time },
    )?;
    let scope = Scope::new(include, exclude);
    let scope_key = serde_json::to_string(&(filter, include, exclude)).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut budget = Budget::default();
    let mut retained_bytes = 0u64;
    let mut out = Plan {
        local_root: local_root.to_path_buf(),
        server_root: server_root.to_path_buf(),
        checked: scan.same_by_content + scan.summary.trivial + scan.summary.differing,
        unverified: scan.summary.unverified,
        state_path: state_path.map(Path::to_path_buf),
        server_binding,
        local_binding,
        validation_client: anc.validation_client(),
        binding_reset,
        metadata: metadata.clone(),
        ..Default::default()
    };
    for pair in &scan.items {
        if scope.skipped_as(&pair.key).is_none() {
            let sizes = [pair.local.as_ref().map_or(0, |s| s.size), pair.server.as_ref().map_or(0, |s| s.size)];
            let working = sizes[0].saturating_add(sizes[1]).saturating_mul(5);
            if sizes.iter().any(|size| *size > limits.max_file_bytes.min(MAX_FILE_BYTES))
                || retained_bytes.saturating_add(working) > limits.max_plan_bytes
            {
                let mut entry = new_entry(pair);
                entry.note = "超过单文件读取或预览内存预算，未读取合并内容，请缩小合并范围".into();
                out.entries.push(entry);
                out.budget_hit = true;
                out.untried += 1;
                continue;
            }
        }
        // Same / Trivial 的对是 BC 口径下的一致，合了也不会有任何字节变化，不必占清单
        if matches!(pair.verdict, Verdict::Same | Verdict::Trivial) {
            if scope.skipped_as(&pair.key).is_none() {
                if let (Some(path), Some(state), Some(local), Some(server)) =
                    (state_path, state.as_mut(), pair.local.as_ref(), pair.server.as_ref())
                {
                    let left = read_bytes(local_root.join(&local.rel)).map_err(|e| format!("读取本地同步快照失败：{e}"))?;
                    let right = read_bytes(server_root.join(&server.rel)).map_err(|e| format!("读取服务器同步快照失败：{e}"))?;
                    if content_equal(&left, &right) {
                        state.remember(path, &pair.key, &right)?;
                    } else {
                        return Err("扫描后文件内容发生变化，请重新预览".into());
                    }
                }
            }
            continue;
        }
        // 预算只管两侧都要问祖先的那批：单侧增删读一个文件就能定，一次 svn 都不问，
        // 让它跟着撞线就是「合并跑完本地侧还是少几个新文件」——按字母序排在后面的那几个
        let two_sided = pair.local.is_some() && pair.server.is_some();
        if two_sided && (start.elapsed() > limits.max_time || budget.calls >= limits.svn_calls) {
            out.budget_hit = true;
            out.untried += 1;
            continue;
        }
        let baseline = match (&state, state_path) {
            (Some(state), Some(path)) => state.baseline(path, &pair.key)?,
            _ => None,
        };
        let known_new = state.as_ref().map(|state| state.complete_scope.as_deref() == Some(scope_key.as_str())
            && (!state.inventory.contains(&pair.key) || state.new_files.contains(&pair.key)));
        if known_new == Some(true) {
            if let Some(state) = state.as_mut() { state.new_files.insert(pair.key.clone()); }
        }
        let mut entry = decide(local_root, server_root, pair, &scope, anc, local_anc, &mut budget, limits,
            baseline.as_deref(), known_new);
        let bytes = [&entry.observed_local, &entry.observed_server, &entry.write_local, &entry.write_server]
            .iter().map(|bytes| bytes.as_ref().map_or(0, |b| b.len() as u64)).sum::<u64>();
        if retained_bytes.saturating_add(bytes) > limits.max_plan_bytes {
            entry = new_entry(pair);
            entry.note = "合并产物超过预览内存预算，未加入写入清单".into();
            out.budget_hit = true;
            out.untried += 1;
        } else { retained_bytes += bytes; }
        if entry.action.is_write() {
            out.observed_states.insert(entry.rel.clone(), (
                local_anc.wc_state(&entry.local_rel, entry.observed_local.is_some()),
                anc.wc_state(&entry.server_rel, entry.observed_server.is_some()),
            ));
        }
        match entry.action {
            Action::SkipNotWhitelisted => out.outside_whitelist += 1,
            Action::SkipBlacklisted => out.blacklisted += 1,
            _ => {}
        }
        out.entries.push(entry);
    }
    if let (Some(state), Some(path)) = (state.as_ref(), state_path) {
        detect_renames(&mut out, state, path, anc, &scope)?;
    }
    let previous_metadata = state.as_ref().and_then(|s| s.metadata.as_ref());
    let mut changed = std::collections::HashSet::new();
    for key in anc.pending_properties().into_iter().chain(local_anc.pending_properties()) { changed.insert(key); }
    if let Some(previous) = previous_metadata {
        for (old, new) in [(&previous.0, &metadata.0), (&previous.1, &metadata.1)] {
            let empty = Metadata::new();
            let (old, new) = (old.as_ref().unwrap_or(&empty), new.as_ref().unwrap_or(&empty));
            for key in old.keys().chain(new.keys()) {
                if old.get(key) != new.get(key) { changed.insert(key.clone()); }
            }
        }
    }
    for key in changed.into_iter().filter(|k| k.is_empty() || scope.skipped_as(k).is_none()) {
        let note = "SVN 属性发生变化；本地是普通目录，本次保留服务端属性，不执行属性覆盖";
        if let Some(entry) = out.entries.iter_mut().find(|e| e.rel == key) {
            entry.note = format!("{}；{note}", entry.note);
        } else {
            out.entries.push(Entry { rel: key.clone(), display: if key.is_empty() { ".".into() } else { key.clone() },
                local_rel: key.clone(), server_rel: key, action: Action::SkipMetadata, ancestor: AncInfo::NotNeeded,
                regions: Vec::new(), local: None, server: None, write_local: None, write_server: None,
                delete_local: false, delete_server: false, observed_local: None, observed_server: None,
                note: note.into() });
        }
    }
    out.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    if let (Some(state), Some(path)) = (state.as_mut(), state_path) {
        state.metadata = Some(metadata);
        state.inventory.extend(scan.items.iter().filter(|p| scope.skipped_as(&p.key).is_none()).map(|p| p.key.clone()));
        if out.unverified == 0 && !out.budget_hit {
            state.complete_scope = Some(scope_key);
        }
        state.save(path)?;
        out.state_revision = state.revision;
    }
    Ok(out)
}

// Ordinary folders cannot carry SVN move history. Recognize exact-content rename
// candidates before deletion/copy jobs escape the preview, and require review.
fn detect_renames(out: &mut Plan, state: &SyncState, path: &Path, _anc: &dyn Ancestor, scope: &Scope) -> Result<(), String> {
    let mut blocked = std::collections::HashSet::new();
    let one_sided: Vec<usize> = out.entries.iter().enumerate().filter(|(_, e)|
        e.observed_local.is_some() != e.observed_server.is_some()).map(|(i, _)| i).collect();
    // A partial preview cannot rule out a rename hidden by its budgets.
    if out.budget_hit || out.unverified > 0 || one_sided.len().saturating_mul(one_sided.len()) > 1_000_000 {
        for &i in &one_sided {
            let e = &mut out.entries[i];
            if e.delete_local || e.delete_server {
                e.action = Action::SkipUndecided;
                e.delete_local = false; e.delete_server = false;
                e.note = "预览或改名检查超过预算，无法排除未扫描的改名，暂停删除；请缩小范围后重试".into();
            }
        }
        return Ok(());
    }
    for (i, old) in out.entries.iter().enumerate() {
        if scope.skipped_as(&old.rel).is_some() { continue; }
        let local_missing = old.observed_local.is_none() && old.observed_server.is_some();
        let server_missing = old.observed_server.is_none() && old.observed_local.is_some();
        if !local_missing && !server_missing { continue; }
        let Some(base) = state.baseline(path, &old.rel)? else { continue; };
        for &j in &one_sided {
            let new = &out.entries[j];
            if i == j || scope.skipped_as(&new.rel).is_some() || state.files.contains_key(&new.rel) { continue; }
            let candidate = if local_missing && new.observed_server.is_none() { new.observed_local.as_deref() }
                else if server_missing && new.observed_local.is_none() { new.observed_server.as_deref() }
                else { None };
            if candidate.is_some_and(|bytes| content_equal(&base, bytes)) {
                blocked.insert(i); blocked.insert(j);
            }
        }
    }
    for i in blocked {
        let entry = &mut out.entries[i];
        entry.action = Action::SkipUndecided;
        entry.write_local = None; entry.write_server = None;
        entry.delete_local = false; entry.delete_server = false;
        entry.note = "检测到可能的文件改名，暂停自动删除和新增；请核对后使用 SVN 改名以保留服务端历史和属性".into();
    }
    Ok(())
}

fn new_entry(pair: &Pair) -> Entry {
    Entry {
        rel: pair.key.clone(),
        display: pair.display().to_owned(),
        local_rel: pair.local.as_ref().map(|s| s.rel.clone()).unwrap_or_else(|| pair.display().to_owned()),
        server_rel: pair.server.as_ref().map(|s| s.rel.clone()).unwrap_or_else(|| pair.display().to_owned()),
        action: Action::SkipUndecided,
        ancestor: AncInfo::NotNeeded,
        regions: Vec::new(),
        local: pair.local.as_ref().map(|s| Finger { size: s.size, modified: s.modified }),
        server: pair.server.as_ref().map(|s| Finger { size: s.size, modified: s.modified }),
        write_local: None,
        write_server: None,
        delete_local: false,
        delete_server: false,
        observed_local: None,
        observed_server: None,
        note: String::new(),
    }
}

fn decide(
    local_root: &Path,
    server_root: &Path,
    pair: &Pair,
    scope: &Scope,
    anc: &dyn Ancestor,
    local_anc: &dyn Ancestor,
    budget: &mut Budget,
    limits: &MergeLimits,
    baseline: Option<&[u8]>,
    known_new: Option<bool>,
) -> Entry {
    let read = |path: PathBuf| read_limited(&path, limits.max_file_bytes.min(MAX_FILE_BYTES));
    let mut e = new_entry(pair);
    // 范围在两格之外就一条都不读、一次 svn 都不问，更不占清单里的写入名额
    if let Some(action) = scope.skipped_as(&pair.key) {
        e.action = action;
        return e;
    }
    match (pair.local.as_ref(), pair.server.as_ref()) {
        (Some(local), None) => {
            let Ok(bytes) = read(local_root.join(&local.rel)) else {
                e.note = "本地侧文件读不到".to_owned();
                return e;
            };
            e.observed_local = Some(bytes.clone());
            // 目标在工作副本里还不存在，rel 沿用本地写法；版本化交给「全部上传」
            e.server_rel = local.rel.clone();
            // 「只有本地有」有两种相反的可能：本地新加的文件（该拷上去）、或服务器端有人删了它
            // （该把本地也删掉）。判据：服务器工作副本里这个路径已计划删除 / 已丢失，或者服务器端
            // 根本没有它（不在状态表、盘上也没有）而本地这份是**版本化**文件——两个工作副本对
            // 「仓库里到底有没有这个路径」各执一词，那就是服务器端把它删了。
            let server_state = anc.wc_state(&local.rel, false);
            let local_state = local_anc.wc_state(&local.rel, true);
            if matches!(server_state, WcState::Conflict | WcState::Other)
                || matches!(local_state, WcState::Conflict)
                || local_state == WcState::Other
                || (local_state == WcState::Deleted && server_state != WcState::Deleted)
            {
                e.note = format!(
                    "单侧文件状态无法安全判定（本地：{}；服务器：{}），先人工处理",
                    local_state.text(), server_state.text()
                );
                return e;
            }
            let server_deleted = server_state == WcState::Deleted || baseline.is_some();
            if server_deleted {
                let old = baseline.map(|b| b.to_vec()).or_else(|| local_anc.base(&local.rel).ok().flatten());
                let Some(old) = old else {
                    e.note = "服务器端缺失，但无法确认本地是否修改，请人工处理删除".into();
                    return e;
                };
                if !content_equal(&bytes, &old) {
                    e.action = Action::SkipConflict;
                    e.note = "服务器已删除、本地仍有修改，需人工决定保留或删除".into();
                    return e;
                }
                e.action = Action::DeleteLocal;
                e.delete_local = true;
                e.note = format!(
                    "服务器端已删除该文件（{}），合并时同步删掉本地副本",
                    server_state.text()
                );
                return e;
            }
            if known_new == Some(false) && local_state != WcState::Added {
                e.note = "首次核对或缺少该文件的同步记录，无法区分新增和删除，请先人工核对至两侧一致".into();
                return e;
            }
            if (local_state == WcState::Normal && known_new != Some(true)) || local_state == WcState::Deleted {
                e.note = "没有上次同步记录，无法区分本地新增和服务器删除，请人工核对".into();
                return e;
            }
            e.action = Action::CopyLocalToServer;
            e.write_server = Some(bytes);
            e
        }
        (None, Some(server)) => {
            let state = anc.wc_state(&server.rel, true);
            if !state.writable() {
                e.action = Action::SkipUndecided;
                e.note = format!("服务器端处于「{}」，不拷到本地侧", state.text());
                return e;
            }
            let local_state = local_anc.wc_state(&server.rel, false);
            if matches!(local_state, WcState::Conflict | WcState::Other) {
                e.note = format!("本地侧处于「{}」，先人工处理", local_state.text());
                return e;
            }
            let Ok(bytes) = read(server_root.join(&server.rel)) else {
                e.note = "服务器端文件读不到".to_owned();
                return e;
            };
            e.observed_server = Some(bytes.clone());
            e.local_rel = server.rel.clone();
            if let Some(old) = baseline {
                if !content_equal(&bytes, old) {
                    e.action = Action::SkipConflict;
                    e.note = "本地已删除、服务器仍有修改，需人工决定保留或删除".into();
                } else {
                    e.action = Action::DeleteServer;
                    e.delete_server = true;
                    e.note = "本地已删除，服务器内容与上次同步一致，同步删除工作副本文件".into();
                }
                return e;
            }
            if local_anc.wc_state(&server.rel, false) == WcState::Deleted {
                e.note = "本地已删除，但没有同步基线，请人工核对，避免复活文件".into();
                return e;
            }
            if known_new == Some(false) && state != WcState::Added {
                e.note = "首次核对或缺少该文件的同步记录，无法区分新增和删除，请先人工核对至两侧一致".into();
                return e;
            }
            e.action = Action::CopyServerToLocal;
            e.write_local = Some(bytes);
            e
        }
        (Some(local), Some(server)) => {
            let state = anc.wc_state(&server.rel, true);
            if !state.writable() {
                e.action = Action::SkipUndecided;
                e.note = format!("服务器端处于「{}」，先人工处理", state.text());
                return e;
            }
            let local_state = local_anc.wc_state(&local.rel, true);
            if !local_state.writable() {
                e.note = format!("本地侧处于「{}」，先人工处理", local_state.text());
                return e;
            }
            let (Ok(local_bytes), Ok(server_bytes)) = (
                read(local_root.join(&local.rel)),
                read(server_root.join(&server.rel)),
            ) else {
                e.note = "任一侧文件读不到".to_owned();
                return e;
            };
            e.observed_local = Some(local_bytes.clone());
            e.observed_server = Some(server_bytes.clone());
            let found = if let Some(bytes) = baseline {
                AncestorOf { bytes: bytes.to_vec(), info: AncInfo::Synced }
            } else {
                ancestor_of(anc, local_anc, &server.rel, &local_bytes, budget, limits)
            };
            if let AncInfo::Unknown(reason) = &found.info {
                e.action = Action::SkipUndecided;
                e.ancestor = AncInfo::Unknown(reason.clone());
                e.note = reason.text();
                return e;
            }
            e.ancestor = found.info.clone();
            let text_tier = local.size.max(server.size) <= NORMALIZE_MAX
                && looks_text(&local_bytes)
                && looks_text(&server_bytes);
            if !text_tier {
                // 整文件档只认两种可证明的情形：本地与基线一字不差（变的只有服务器），
                // 或服务器端就是基线（变的只有本地）。反向的「服务器没动」按字节判，
                // 是因为反向一旦判错就会把别人未提交的改动覆盖掉
                if found.bytes == local_bytes && local_bytes != server_bytes {
                    e.action = Action::OverwriteServerToLocal;
                    e.write_local = Some(server_bytes);
                } else if found.bytes == server_bytes && local_bytes != server_bytes {
                    e.action = Action::OverwriteLocalToServer;
                    e.write_server = Some(local_bytes);
                } else {
                    e.action = Action::SkipBinary;
                    e.note = "非文本文件，两边都相对基线变过".to_owned();
                }
                return e;
            }
            let m = three_way_limited(&found.bytes, &local_bytes, &server_bytes, limits.lcs_cell_limit);
            if m.too_big {
                // 太大跑不动精确 LCS：退一步只认「可证明」的单方改写，与二进制分支同一道保障；
                // 两侧都相对基线变过才真正留给人工，避免「文件大就草率整文件跳过」
                if found.bytes == local_bytes && local_bytes != server_bytes {
                    e.action = Action::OverwriteServerToLocal;
                    e.write_local = Some(server_bytes);
                } else if found.bytes == server_bytes && local_bytes != server_bytes {
                    e.action = Action::OverwriteLocalToServer;
                    e.write_server = Some(local_bytes);
                } else {
                    e.action = Action::SkipUndecided;
                    e.note = "行数规模超过 LCS 上限，且两侧都改过，需人工处理".to_owned();
                }
                return e;
            }
            if m.has_conflict() {
                e.action = Action::SkipConflict;
                e.regions = m.regions.clone();
                e.note = "同一处两边都改过，整文件不动".to_owned();
                return e;
            }
            e.regions = m.regions.clone();
            // 各自保留自己的 BOM 与行尾，所以合完的字节要和这一侧的现状比，不能比对面
            e.write_local = m.merged_local.filter(|bytes| *bytes != local_bytes);
            e.write_server = m.merged_server.filter(|bytes| *bytes != server_bytes);
            e.action = if e.write_local.is_none() && e.write_server.is_none() {
                Action::SkipUnchanged
            } else {
                Action::MergeBoth
            };
            e
        }
        (None, None) => e,
    }
}

// -------------------------------------------------------------------- 报告

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
        human_bytes(plan.backup_bytes())
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

// ---------------------------------------------------------------------- 落盘

/// 一条落盘任务：目标侧、该侧自己的 rel、绝对路径与要写的字节。
/// `apply` 只认这里，不回读 `Entry.rel`（那是小写配对键，磁盘写法可能只差大小写）
#[derive(Clone, Debug)]
pub struct Job<'a> {
    pub target: Target,
    pub rel: String,
    pub dest: PathBuf,
    pub bytes: &'a [u8],
    /// 预览时该目标的指纹；`None` = 预览时还不存在（新增），落盘时只许继续不存在
    pub before: Option<Finger>,
    /// true = 删掉 `dest`（服务器端已删除、本地跟着删）；`bytes` 为空，不再写内容
    pub delete: bool,
}

/// 计划里真正要写的东西，按文件分组：一个文件的两侧先都备份完才开写
pub fn pending(plan: &Plan) -> Vec<(String, Vec<Job<'_>>)> {
    let mut groups: Vec<(String, Vec<Job<'_>>)> = Vec::new();
    for entry in &plan.entries {
        let mut jobs = Vec::new();
        if entry.delete_local {
            jobs.push(Job {
                target: Target::Local,
                rel: entry.local_rel.clone(),
                dest: plan.local_root.join(&entry.local_rel),
                bytes: &[],
                before: entry.local,
                delete: true,
            });
        }
        if entry.delete_server {
            jobs.push(Job {
                target: Target::Server,
                rel: entry.server_rel.clone(),
                dest: plan.server_root.join(&entry.server_rel),
                bytes: &[],
                before: entry.server,
                delete: true,
            });
        }
        if let Some(bytes) = &entry.write_local {
            jobs.push(Job {
                target: Target::Local,
                rel: entry.local_rel.clone(),
                dest: plan.local_root.join(&entry.local_rel),
                bytes: bytes.as_slice(),
                before: entry.local,
                delete: false,
            });
        }
        if let Some(bytes) = &entry.write_server {
            jobs.push(Job {
                target: Target::Server,
                rel: entry.server_rel.clone(),
                dest: plan.server_root.join(&entry.server_rel),
                bytes: bytes.as_slice(),
                before: entry.server,
                delete: false,
            });
        }
        if !jobs.is_empty() {
            groups.push((entry.rel.clone(), jobs));
        }
    }
    groups
}

/// 一次落盘的账，直接进输出日志
#[derive(Clone, Debug, Default)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Applied {
    /// 这次会话的备份目录；选了「直接合并」就没有还原点，这里是 `None`
    pub backup: Option<PathBuf>,
    pub written: usize,
    pub bytes: u64,
    pub failed: Vec<String>,
}

/// 还原的结果
#[derive(Clone, Debug, Default)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Restored {
    pub restored: usize,
    pub removed: usize,
    pub failed: Vec<String>,
}

/// 一次会话备份的清单，给「还原上一次合并」那扇窗列出来
#[derive(Clone, Debug)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct BackupInfo {
    pub dir: PathBuf,
    pub name: String,
    pub files: usize,
    pub bytes: u64,
    pub unfinished: bool,
}

const BACKUPS_KEPT: usize = 5;
const JOURNAL: &str = "journal.jsonl";
pub const BACKUP_PREFIX: &str = "merge-backup-";

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

static TEMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// 一次会话的备份目录：`<备份根>/merge-backup-<时间戳>`。根目录由调用方给
/// （程序里是配置目录，测试里是临时目录），这一层不自己找路
fn backup_dir(root: &Path, session: &str) -> PathBuf {
    root.join(format!("{BACKUP_PREFIX}{session}"))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    side: String,
    #[serde(default)]
    rel: String,
    #[serde(default)]
    dest: String,
    #[serde(default)]
    fresh: bool,
    /// 备份时（也就是写入前）的指纹
    #[serde(default)]
    size: u64,
    #[serde(default)]
    modified: u64,
    /// 写入字节数；仅保留作日志兼容，撤销依据原文与产物的准确内容。
    #[serde(default)]
    written: u64,
    /// true = 这一条是「我们删掉了它」，还原时要按「现在仍然不存在」来还原
    #[serde(default)]
    deleted: bool,
    /// 新日志额外保留精确的写入产物，撤销不能只核对文件大小。
    #[serde(default)]
    exact_after: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct JournalLine {
    kind: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    rel: String,
    #[serde(default)]
    writes: Vec<Record>,
    #[serde(default)]
    state_path: Option<PathBuf>,
    #[serde(default)]
    local_root: PathBuf,
    #[serde(default)]
    server_root: PathBuf,
    #[serde(default)]
    bindings: Option<(Option<RepositoryBinding>, Option<RepositoryBinding>)>,
}

fn append_journal(dir: &Path, line: &JournalLine) -> Result<(), String> {
    use std::io::BufWriter;
    let path = dir.join(JOURNAL);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("写不进合并日志 {}：{e}", path.display()))?;
    let mut writer = BufWriter::new(&mut file);
    serde_json::to_writer(&mut writer, line).map_err(|e| format!("合并日志写坏了：{e}"))?;
    writer.write_all(b"\n").map_err(|e| format!("合并日志写坏了：{e}"))?;
    writer.flush().map_err(|e| format!("合并日志写坏了：{e}"))?;
    drop(writer);
    file.sync_all().map_err(|e| format!("合并日志未落盘：{e}"))
}

fn read_observed(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("无法确认 {} 的当前状态：{e}", path.display())),
        Ok(meta) if !meta.file_type().is_file() => Err(format!("目标不是普通文件：{}", path.display())),
        Ok(_) => read_bytes(path).map(Some).map_err(|e| format!("无法核对 {}：{e}", path.display())),
    }
}

fn verify_observed(plan: &Plan, entry: &Entry, local: Option<&[u8]>, server: Option<&[u8]>) -> Result<(), String> {
    if !plan.local_root.is_dir() || !plan.server_root.is_dir() {
        return Err("绑定目录已不可用，不能将目录失联当成文件删除".into());
    }
    if read_observed(&plan.local_root.join(&entry.local_rel))?.as_deref() != local
        || read_observed(&plan.server_root.join(&entry.server_rel))?.as_deref() != server
    {
        return Err("预览之后文件被改过、新增或删除，请重新预览".into());
    }
    Ok(())
}

fn read_journal(dir: &Path) -> Result<Vec<JournalLine>, String> {
    let raw = fs::read_to_string(dir.join(JOURNAL)).map_err(|e| format!("无法读取备份日志 {}：{e}", dir.display()))?;
    let mut lines = Vec::new();
    for (index, line) in raw.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let parsed = serde_json::from_str::<JournalLine>(line)
            .map_err(|e| format!("备份日志第 {} 行已损坏，未执行还原：{e}", index + 1))?;
        if !matches!(parsed.kind.as_str(), "session" | "begin" | "done" | "finished" | "restore_begin" | "restored" | "restore_partial") {
            return Err(format!("备份日志第 {} 行类型未知，未执行还原", index + 1));
        }
        if parsed.kind == "begin" && (parsed.rel.is_empty() || parsed.writes.is_empty()
            || parsed.writes.iter().any(|record| !matches!(record.side.as_str(), "local" | "server")
                || record.rel.is_empty() || !Path::new(&record.dest).is_absolute()
                || !Path::new(&record.rel).components().all(|c| matches!(c, std::path::Component::Normal(_)))))
        {
            return Err(format!("备份日志第 {} 行文件记录不完整或路径无效，未执行还原", index + 1));
        }
        lines.push(parsed);
    }
    Ok(lines)
}

/// 原子替换：临时文件写在目标自己的父目录里（跨盘 rename 不是原子操作），
/// 写完 sync 再 rename；被编辑器占用时 rename 失败，删掉临时文件并报失败，不盲目重试
fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    replace_mode(path, bytes, true)
}

fn replace_mode(path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), String> {
    if fs::metadata(path).map(|m| m.permissions().readonly()).unwrap_or(false) {
        return Err(format!("目标是只读文件：{}", path.display()));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent).map_err(|e| format!("建不出目录 {}：{e}", parent.display()))?;
    let temp = parent.join(format!(
        ".repomanager-merge-tmp-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut file =
            File::create(&temp).map_err(|e| format!("写不出临时文件 {}：{e}", temp.display()))?;
        file.write_all(bytes).map_err(|e| format!("写不出临时文件：{e}"))?;
        file.sync_all().map_err(|e| format!("临时文件没落稳：{e}"))?;
    }
    let installed = if overwrite { fs::rename(&temp, path) } else {
        // 同目录临时文件与目标同卷；硬链接创建是“不覆盖已有目标”的原子操作。
        fs::hard_link(&temp, path).map(|()| { let _ = fs::remove_file(&temp); })
    };
    match installed {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(format!("替换 {} 失败：{e}", path.display()))
        }
    }
}

fn read_lease(path: &Path) -> Result<Option<File>, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(5); // 保留原子替换能力，同时阻止普通写句柄。
    }
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("锁定 {} 失败：{e}", path.display())),
    }
}

fn change_if_unchanged(path: &Path, expected: Option<&[u8]>, replacement: Option<&[u8]>) -> Result<(), String> {
    let _lease = read_lease(path)?;
    if read_observed(path)?.as_deref() != expected { return Err("文件内容已改变，停止写入或回退".into()) }
    crate::merge_io::change(path, expected, replacement)
}

#[cfg(test)]
thread_local! { static FAIL_WRITE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }

fn write_job(job: &Job<'_>, original: Option<&[u8]>) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_WRITE.with(|failure| failure.borrow().as_ref() == Some(&job.dest)) {
        return Err("模拟目标写入失败".into());
    }
    change_if_unchanged(&job.dest, original, if job.delete { None } else { Some(job.bytes) })
}

/// 落盘。每个目标先指纹重查（预览是建议不是锁）。
///
/// `backup` 为真时，每个文件两边都先整份复制进会话目录、逐条记 journal，之后能按 journal 还原；
/// 为假就是用户自己核对过、不留还原点的「直接合并」——指纹重查、只读拒绝一律照旧，只是不再备份。
pub fn apply(plan: &Plan, root: &Path, backup: bool) -> Result<Applied, String> {
    if !plan.local_root.is_dir() || !plan.server_root.is_dir() {
        return Err("绑定目录已不可用，请恢复目录连接后重新预览".into());
    }
    if let Some(client) = &plan.validation_client {
        let server = crate::svn::SvnAncestor::new(client.clone(), &plan.server_root);
        let local = crate::svn::SvnAncestor::new_local(client.clone(), &plan.local_root);
        if let Some(reason) = server.error().or_else(|| local.error()) {
            return Err(format!("无法复核工作副本状态：{reason}"));
        }
        if server.binding()? != plan.server_binding || local.binding()? != plan.local_binding {
            return Err("预览后 SVN 仓库或分支已改变，请重新预览".into());
        }
        if (local.metadata()?, server.metadata()?) != plan.metadata {
            return Err("预览后 SVN 属性已改变，请重新预览".into());
        }
        for entry in plan.entries.iter().filter(|e| e.action.is_write()) {
            let current = (local.wc_state(&entry.local_rel, entry.observed_local.is_some()),
                server.wc_state(&entry.server_rel, entry.observed_server.is_some()));
            if plan.observed_states.get(&entry.rel) != Some(&current) {
                return Err(format!("{}：预览后 SVN 状态已改变，请重新预览", entry.display));
            }
        }
    }
    let _state_lock = plan.state_path.as_deref().map(lock_state).transpose()?;
    let mut state = plan.state_path.as_deref().map(|p| SyncState::load(p, &plan.local_root, &plan.server_root)).transpose()?;
    if state.as_ref().is_some_and(|state| state.revision != plan.state_revision) {
        return Err("同步基线在预览后已改变，请重新预览".into());
    }
    let groups = pending(plan);
    let entries: HashMap<&str, &Entry> = plan.entries.iter().map(|e| (e.rel.as_str(), e)).collect();
    let session = format!("{}-{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S-%f"),
        std::process::id(), TEMP_SEQ.fetch_add(1, Ordering::Relaxed));
    let dir = backup_dir(root, &session);
    // 没备份就没这本账：journal 只在备份目录存在时才写
    let log = |line: &JournalLine| if backup { append_journal(&dir, line) } else { Ok(()) };
    if backup {
        fs::create_dir_all(root).map_err(|e| format!("建不出备份根目录：{e}"))?;
        fs::create_dir(&dir).map_err(|e| format!("建不出独立备份目录：{e}"))?;
    }
    let mut out = Applied { backup: backup.then(|| dir.clone()), ..Default::default() };
    log(&JournalLine { kind: "session".into(), session: session.clone(), state_path: plan.state_path.clone(),
        local_root: plan.local_root.clone(), server_root: plan.server_root.clone(),
        bindings: plan.validation_client.as_ref().map(|_| (plan.local_binding.clone(), plan.server_binding.clone())),
        ..Default::default() })?;
    if backup {
        if let (Some(state), Some(path)) = (state.as_mut(), plan.state_path.as_deref()) {
            state.backup_root = Some(root.to_path_buf());
            state.backup_sessions.insert(dir.file_name().unwrap().to_string_lossy().into_owned());
            state.save(path)?;
        }
    }
    for (rel, jobs) in &groups {
        let entry = *entries.get(rel.as_str()).ok_or("合并清单中缺少来源记录")?;
        let mut expected_local = entry.observed_local.clone();
        let mut expected_server = entry.observed_server.clone();
        let leases = [read_lease(&plan.local_root.join(&entry.local_rel)), read_lease(&plan.server_root.join(&entry.server_rel))];
        if let Some(reason) = leases.iter().find_map(|lease| lease.as_ref().err()) {
            out.failed.push(format!("{rel}：{reason}"));
            continue;
        }
        if let Err(reason) = verify_observed(plan, entry, expected_local.as_deref(), expected_server.as_deref()) {
            out.failed.push(format!("{rel}：{reason}"));
            continue;
        }
        let mut records: Vec<Record> = Vec::new();
        let mut stale = None;
        for job in jobs {
            if fs::metadata(&job.dest).is_ok_and(|m| m.permissions().readonly()) {
                stale = Some(format!("目标是只读文件：{}", job.dest.display()));
                break;
            }
            let current = Finger::now(&job.dest);
            match (job.before, current) {
                (None, Some(_)) => stale = Some("预览之后目标已存在".to_owned()),
                (Some(finger), current) if current != Some(finger) => {
                    stale = Some("预览之后文件被改过".to_owned())
                }
                _ => {}
            }
            if stale.is_some() {
                break;
            }
            let fresh = current.is_none();
            if backup && !fresh {
                let slot = dir.join(Target::dir(job.target)).join(&job.rel);
                let raw = read_bytes(&job.dest)
                    .map_err(|e| format!("备份读不到 {}：{e}", job.dest.display()))?;
                replace(&slot, &raw)?;
            }
            if backup && !job.delete {
                replace(&dir.join("after").join(job.target.dir()).join(&job.rel), &job.bytes)?;
            }
            records.push(Record {
                side: job.target.dir().to_owned(),
                rel: job.rel.clone(),
                dest: job.dest.display().to_string(),
                fresh,
                size: current.map(|f| f.size).unwrap_or_default(),
                modified: current.map(|f| f.modified).unwrap_or_default(),
                written: job.bytes.len() as u64,
                deleted: job.delete,
                exact_after: true,
            });
        }
        if let Some(reason) = stale {
            out.failed.push(format!("{}：{reason}", rel.replace('/', "\\")));
            continue;
        }
        log(
            &JournalLine { kind: "begin".into(), rel: rel.clone(), writes: records.clone(), ..Default::default() },
        )?;
        let mut done: Vec<Record> = Vec::new();
        let mut group_failed = false;
        for (job, record) in jobs.iter().zip(records.iter()) {
            if let Err(reason) = verify_observed(plan, entry, expected_local.as_deref(), expected_server.as_deref()) {
                out.failed.push(format!("{rel}：{reason}"));
                group_failed = true;
                break;
            }
            let original = if job.target == Target::Local { entry.observed_local.as_deref() } else { entry.observed_server.as_deref() };
            let result = write_job(job, original);
            match result {
                Ok(()) => {
                    let expected = if job.target == Target::Local { &mut expected_local } else { &mut expected_server };
                    *expected = if job.delete { None } else { Some(job.bytes.to_vec()) };
                    let after = Finger::now(&job.dest)
                        .unwrap_or(Finger { size: job.bytes.len() as u64, modified: 0 });
                    let mut line = record.clone();
                    line.size = after.size;
                    line.modified = after.modified;
                    done.push(line);
                    out.written += 1;
                    out.bytes += job.bytes.len() as u64;
                }
                Err(err) => {
                    out.failed.push(format!("{}：{err}", rel.replace('/', "\\")));
                    group_failed = true;
                    break;
                }
            }
        }
        if group_failed {
            for job in jobs.iter().take(done.len()).rev() {
                let expected = if job.delete { None } else { Some(job.bytes) };
                let original = if job.target == Target::Local { entry.observed_local.as_deref() } else { entry.observed_server.as_deref() };
                let rolled_back = change_if_unchanged(&job.dest, expected, original);
                match rolled_back {
                    Ok(()) => {
                        out.written -= 1;
                        out.bytes -= job.bytes.len() as u64;
                        let side = if job.target == Target::Local { &mut expected_local } else { &mut expected_server };
                        *side = original.map(<[u8]>::to_vec);
                    }
                    Err(error) => out.failed.push(format!("{rel}：单文件回退失败，保留备份供还原：{error}")),
                }
            }
        }
        log(
            &JournalLine { kind: "done".into(), rel: rel.clone(), writes: done, ..Default::default() },
        )?;
        if let (Some(state), Some(path)) = (state.as_mut(), plan.state_path.as_deref()) {
            // 只在两侧仍是这次观察/写入的内容且已经收敛时推进基线。
            if verify_observed(plan, entry, expected_local.as_deref(), expected_server.as_deref()).is_ok() {
                match (expected_local.as_deref(), expected_server.as_deref()) {
                    (Some(left), Some(right)) if content_equal(left, right) => {
                        state.remember(path, rel, right)?;
                    }
                    (None, None) => {
                        state.files.remove(rel);
                        state.new_files.remove(rel);
                    }
                    _ => {}
                }
            }
        }
    }
    if let (Some(state), Some(path)) = (state.as_mut(), plan.state_path.as_deref()) {
        state.save(path)?;
    }
    // 「直接合并」不留还原点：finished 与只留最近几次的清理都只在备份过时才跑
    if backup {
        log(&JournalLine { kind: "finished".into(), ..Default::default() })?;
        prune(root, BACKUPS_KEPT);
    }
    Ok(out)
}

/// 历次备份，新的在前
pub fn backups(root: &Path) -> Vec<BackupInfo> {
    let Ok(entries) = fs::read_dir(root) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(BACKUP_PREFIX) {
            continue;
        }
        let dir = entry.path();
        let journal = read_journal(&dir);
        let corrupt = journal.is_err();
        let lines = journal.unwrap_or_default();
        let files = lines.iter().filter(|l| l.kind == "begin").count();
        let bytes = fs::read_dir(&dir)
            .map(|root| {
                root.flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| sum_bytes(&e.path()))
                    .sum()
            })
            .unwrap_or_default();
        let finished = !corrupt && lines.iter().any(|l| l.kind == "finished" || l.kind == "restored");
        out.push(BackupInfo {
            name: if corrupt { format!("{name}（日志不可读）") } else { name.clone() },
            dir,
            files,
            bytes,
            unfinished: !finished,
        });
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

fn sum_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries.flatten().fold(0u64, |acc, entry| {
        let path = entry.path();
        if path.is_dir() {
            acc + sum_bytes(&path)
        } else {
            acc + fs::metadata(&path).map(|m| m.len()).unwrap_or_default()
        }
    })
}

/// 最近一次没写完的备份；界面上从历次备份清单里挑，这里只给测试用
#[cfg(test)]
pub fn unfinished_backup(root: &Path) -> Option<BackupInfo> {
    backups(root).into_iter().find(|b| b.unfinished)
}

/// 只留最近 `keep` 次备份。时间戳目录名按字典序就是按时间序
fn prune(root: &Path, keep: usize) {
    let all = backups(root);
    if all.len() <= keep {
        return;
    }
    for old in all.iter().filter(|b| !b.unfinished).skip(keep) {
        if recovery_acknowledged(&old.dir) { let _ = fs::remove_dir_all(&old.dir); }
    }
}

/// 清掉配置目录里历次合并留下的备份目录，返回删掉的目录数与释放的字节数。
/// 还原点是这些目录唯一的用处，删完就退不回合并前，所以确认与提示都归调用方；
/// 写到一半断掉的那次（`unfinished`）不动——两侧还不一致，删了它就没有修回来的凭据。
pub fn clean_backups(root: &Path) -> (usize, u64) {
    let mut removed = 0;
    let mut bytes = 0;
    for item in backups(root).iter().filter(|item| !item.unfinished) {
        if recovery_acknowledged(&item.dir) && fs::remove_dir_all(&item.dir).is_ok() {
            removed += 1;
            bytes += item.bytes;
        }
    }
    (removed, bytes)
}

/// 按 journal 还原：当前字节仍与本次产物匹配时才覆盖或删除。
#[cfg(test)]
pub fn restore(dir: &Path) -> Result<Restored, String> { restore_with_client(dir, None) }

pub fn restore_with_client(dir: &Path, client: Option<&crate::svn::Svn>) -> Result<Restored, String> {
    let lines = read_journal(dir)?;
    if lines.is_empty() {
        return Err(format!("{} 里没有可还原的合并日志", dir.display()));
    }
    let session = lines.iter().find(|l| l.kind == "session");
    if session.is_none() || lines.iter().filter(|l| l.kind == "session").count() != 1 {
        return Err("备份日志缺少唯一的会话头，未执行还原".into());
    }
    if let Some((local_binding, server_binding)) = &session.unwrap().bindings {
        let client = client.ok_or("此备份需要 SVN 身份校验，请配置 SVN 后再还原")?;
        let local = crate::svn::SvnAncestor::new_local(client.clone(), &session.unwrap().local_root);
        let server = crate::svn::SvnAncestor::new(client.clone(), &session.unwrap().server_root);
        if local.binding()? != *local_binding || server.binding()? != *server_binding {
            return Err("当前 SVN 仓库或分支与备份不一致，未执行还原".into());
        }
    }
    for line in lines.iter().filter(|l| l.kind == "begin") {
        if line.rel.is_empty() || line.writes.is_empty() {
            return Err("备份日志文件记录不完整，未执行还原".into());
        }
        for record in &line.writes {
            if !matches!(record.side.as_str(), "local" | "server") || record.rel.is_empty()
                || !Path::new(&record.rel).components().all(|c| matches!(c, std::path::Component::Normal(_)))
                || !Path::new(&record.dest).is_absolute()
            { return Err("备份日志文件路径无效，未执行还原".into()) }
        }
    }
    let state_path = session.and_then(|s| s.state_path.as_deref());
    let _state_lock = state_path.map(lock_state).transpose()?;
    // 先将失效凭据写入备份自身，恢复不依赖 state.json 能否读取/写入。
    append_journal(dir, &JournalLine { kind: "restore_begin".into(), state_path: state_path.map(Path::to_path_buf), ..Default::default() })?;
    let mut out = Restored::default();
    for line in lines.iter().filter(|l| l.kind == "begin") {
        for record in &line.writes {
            let dest = PathBuf::from(&record.dest);
            let current = match read_observed(&dest) {
                Ok(bytes) => bytes,
                Err(e) => { out.failed.push(e); continue; }
            };
            let slot = dir.join(&record.side).join(&record.rel);
            if record.deleted {
                // 我们删掉的文件：只有它还保持「不存在」才还原，期间被重新建过的不碰
                let Ok(raw) = read_bytes(&slot) else {
                    out.failed.push(format!("{}：备份文件不见了", dest.display()));
                    continue;
                };
                if current.as_deref() == Some(raw.as_slice()) {
                    continue; // 删除未执行，或已还原，不重复覆盖。
                }
                if current.is_some() {
                    out.failed.push(format!("{}：删除之后又被改过，不还原", dest.display()));
                    continue;
                }
                match change_if_unchanged(&dest, None, Some(&raw)) {
                    Ok(()) => out.restored += 1,
                    Err(e) => out.failed.push(e),
                }
                continue;
            }
            let after = if record.exact_after {
                match read_bytes(dir.join("after").join(&record.side).join(&record.rel)) {
                    Ok(bytes) => Some(bytes),
                    Err(e) => {
                        out.failed.push(format!("{}：写入产物快照读不到（{e}），不还原", dest.display()));
                        continue;
                    }
                }
            } else { None };
            if record.fresh {
                match current.as_deref() {
                    None => continue,
                    Some(bytes) if after.as_deref() != Some(bytes) => {
                        out.failed.push(format!("{}：预览之后又被改过，不删", dest.display()));
                        continue;
                    }
                    Some(_) => {}
                }
                match change_if_unchanged(&dest, current.as_deref(), None) {
                    Ok(()) => out.removed += 1,
                    Err(e) => out.failed.push(format!("{}：删不掉（{e}）", dest.display())),
                }
                continue;
            }
            let Ok(raw) = read_bytes(&slot) else {
                out.failed.push(format!("{}：备份文件不见了", dest.display()));
                continue;
            };
            // 还原只动「备份时那份」或「我们写出去那份」，别人后来改过的不碰
            let untouched = current.as_deref() == Some(raw.as_slice());
            let is_ours = after.is_some() && current.as_deref() == after.as_deref();
            if !untouched && !is_ours {
                out.failed.push(format!("{}：合并之后又被改过，不覆盖", dest.display()));
                continue;
            }
            match change_if_unchanged(&dest, current.as_deref(), Some(&raw)) {
                Ok(()) => out.restored += 1,
                Err(e) => out.failed.push(e),
            }
        }
    }
    append_journal(
        dir,
        &JournalLine { kind: if out.failed.is_empty() { "restored" } else { "restore_partial" }.into(),
            session: chrono::Local::now().to_rfc3339(), ..Default::default() },
    )?;
    if let (Some(session), Some(path)) = (session, state_path) {
        if let Ok(mut state) = SyncState::load(path, &session.local_root, &session.server_root) {
            for line in lines.iter().filter(|l| l.kind == "begin") {
                state.files.remove(&line.rel);
                state.new_files.remove(&line.rel);
            }
            state.recovery_seen.insert(dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
            let _ = state.save(path); // 失败时保留恢复记录，下次预览会先撤销这些凭据。
        }
    }
    Ok(out)
}

fn recovery_acknowledged(dir: &Path) -> bool {
    let Ok(lines) = read_journal(dir) else { return false };
    let Some(marker) = lines.iter().find(|l| l.kind == "restore_begin") else { return true };
    let Some(path) = &marker.state_path else { return true };
    let Ok(raw) = read_bytes(path) else { return false };
    let Ok(state) = serde_json::from_slice::<SyncState>(&raw) else { return false };
    state.recovery_seen.contains(dir.file_name().unwrap_or_default().to_string_lossy().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行数组 → LF 文本，末尾带换行：多数用例只关心行内容
    fn text(lines: &[&str]) -> String {
        let mut s = String::new();
        for line in lines {
            s.push_str(line);
            s.push('\n');
        }
        s
    }

    /// 同样的行、CRLF 行尾，可选带 BOM：用来验「合并各留自己的行尾与 BOM」
    fn windows_text(lines: &[&str], bom: bool) -> String {
        let mut s = String::new();
        if bom {
            s.push('\u{FEFF}');
        }
        for line in lines {
            s.push_str(line);
            s.push_str("\r\n");
        }
        s
    }

    fn keys(raw: &[u8]) -> Vec<String> {
        let parsed = FileText::parse(raw);
        parsed.keys().iter().map(|k| String::from_utf8_lossy(k).into_owned()).collect()
    }

    /// 合并的成败判据：两侧都收敛成 `want`，且按 BC 口径彼此相等
    fn assert_converges(anc: &str, local: &str, server: &str, want: &[&str]) {
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert!(!m.too_big);
        assert!(!m.has_conflict(), "regions = {:?}", m.regions);
        let (Some(l), Some(s)) = (m.merged_local.as_ref(), m.merged_server.as_ref()) else {
            panic!("两侧都该产出字节：{:?}", m.regions);
        };
        let want: Vec<String> = want.iter().map(|w| w.to_string()).collect();
        assert_eq!(keys(l), want, "本地侧");
        assert_eq!(keys(s), want, "服务器侧");
        assert!(content_equal(l, s));
    }

    #[test]
    fn parse_render_is_the_identity() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            b"a\nb\nc\n".to_vec(),
            b"a\r\nb\r\nc\r\n".to_vec(),
            b"a\rb\r".to_vec(),
            b"a\nb".to_vec(),
            b"\n".to_vec(),
            b"a\n\n\nb\n".to_vec(),
            b"\xef\xbb\xbfa\n".to_vec(),
            // 同一份文件里混着三种行尾也要一字不差地拼回去
            b"a\r\nb\nc\r\nd".to_vec(),
        ];
        for raw in cases {
            assert_eq!(FileText::parse(&raw).render(), raw, "{}", String::from_utf8_lossy(&raw));
        }
    }

    #[test]
    fn gbk_trail_bytes_never_split_a_line() {
        // GBK 尾字节落在 0x40–0xFE，取不到 0x0A 也取不到 0x0D，所以按字节切行对
        // UTF-8 / GBK 混存的仓库都安全。这里刻意塞一个尾字节正好是 0x40（'@'）的汉字。
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(b"// ");
        raw.extend_from_slice(&[0xD6, 0xD0, 0xC4, 0xE3]); // 中文
        raw.extend_from_slice(&[0xB0, 0x40]); // 尾字节 = '@'
        raw.push(b'\n');
        raw.extend_from_slice(b"tail\n");
        let parsed = FileText::parse(&raw);
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.render(), raw);
        assert!(content_equal(&raw, &raw));
    }

    #[test]
    fn lcs_ops_shape_a_replacement_as_one_change() {
        let (a_raw, b_raw) = (text(&["x", "y", "z"]), text(&["x", "Q", "z"]));
        let (fa, fb) = (FileText::parse(a_raw.as_bytes()), FileText::parse(b_raw.as_bytes()));
        let changes = changes_from_ops(&lcs_ops(&fa.keys(), &fb.keys()));
        assert_eq!(changes, vec![Change { anc: 1, len: 1, side: 1, side_len: 1 }]);
    }

    /// 线性空间的 Hirschberg 配一个朴素 O(nm) DP 当对照：裁公共前后缀那一步很容易把配上的行
    /// 丢掉（就出过「整个尾巴没进编辑序列」的事），只靠业务用例未必看得见
    #[test]
    fn lcs_pairs_match_the_naive_dp() {
        fn dp_len(a: &[&[u8]], b: &[&[u8]]) -> usize {
            let mut prev = vec![0usize; b.len() + 1];
            for row in a {
                let mut cur = vec![0usize; b.len() + 1];
                for (j, col) in b.iter().enumerate() {
                    cur[j + 1] = if row == col { prev[j] + 1 } else { prev[j + 1].max(cur[j]) };
                }
                prev = cur;
            }
            prev[b.len()]
        }
        let cases: Vec<(Vec<&str>, Vec<&str>)> = vec![
            (vec!["a", "b", "c"], vec!["a", "Q", "c"]),
            (vec!["a", "b", "c"], vec!["a", "b", "c"]),
            (vec!["a", "b", "c"], vec!["c", "b", "a"]),
            (vec!["x", "y", "z"], vec!["y", "x", "w", "z"]),
            (vec!["1", "2", "3", "4", "5"], vec!["2", "4", "6"]),
            (vec![], vec!["a"]),
            (vec!["a", "a", "a"], vec!["a", "a"]),
            (vec!["整篇", "只有", "尾巴", "相同"], vec!["别的", "尾巴", "相同"]),
        ];
        for (la, lb) in cases {
            let (ra, rb) = (text(&la), text(&lb));
            let (fa, fb) = (FileText::parse(ra.as_bytes()), FileText::parse(rb.as_bytes()));
            let (ka, kb) = (fa.keys(), fb.keys());
            let pairs = lcs_pairs(&ka, &kb);
            assert_eq!(pairs.len(), dp_len(&ka, &kb), "{la:?} vs {lb:?}");
            assert!(pairs.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1), "{la:?} vs {lb:?}");
            assert!(pairs.iter().all(|&(i, j)| ka[i] == kb[j]), "{la:?} vs {lb:?}");
        }
    }

    #[test]
    fn disjoint_edits_merge_both_ways() {
        let anc = text(&["l1", "l2", "l3", "l4", "l5"]);
        let local = text(&["l1", "LOCAL", "l3", "l4", "l5"]);
        let server = text(&["l1", "l2", "l3", "SERVER", "l5"]);
        assert_converges(&anc, &local, &server, &["l1", "LOCAL", "l3", "SERVER", "l5"]);
    }

    #[test]
    fn regions_carry_the_line_spans_the_preview_shows() {
        let anc = text(&["l1", "l2", "l3", "l4", "l5"]);
        let local = text(&["l1", "LOCAL", "l3", "l4", "l5"]);
        let server = text(&["l1", "l2", "l3", "SERVER", "l5"]);
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert_eq!(
            m.regions,
            vec![
                Region { anc: (1, 2), local: Some((1, 2)), server: None, kind: RegionKind::LocalOnly },
                Region { anc: (3, 4), local: None, server: Some((3, 4)), kind: RegionKind::ServerOnly },
            ]
        );
    }

    #[test]
    fn same_line_changed_both_ways_is_a_conflict_and_writes_nothing() {
        let anc = text(&["a", "b", "c"]);
        let local = text(&["a", "L", "c"]);
        let server = text(&["a", "S", "c"]);
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert_eq!(m.regions.len(), 1);
        assert_eq!(m.regions[0].kind, RegionKind::Conflict);
        assert!(m.merged_local.is_none() && m.merged_server.is_none());
    }

    #[test]
    fn identical_edit_is_applied_once() {
        let anc = text(&["a", "b", "c"]);
        let both = text(&["a", "X", "c"]);
        let m = three_way(anc.as_bytes(), both.as_bytes(), both.as_bytes());
        assert_eq!(m.regions.len(), 1);
        assert_eq!(m.regions[0].kind, RegionKind::BothSame);
        assert_eq!(m.merged_local.as_deref(), Some(both.as_bytes()));
        assert_eq!(m.merged_server.as_deref(), Some(both.as_bytes()));

        // 同一处改动、各写各的行尾与 BOM：也算同一次改动，不报冲突
        let crlf = windows_text(&["a", "X", "c"], true);
        let m2 = three_way(anc.as_bytes(), crlf.as_bytes(), both.as_bytes());
        assert_eq!(m2.regions[0].kind, RegionKind::BothSame);
        assert_eq!(m2.merged_local.as_deref(), Some(crlf.as_bytes()));
    }

    #[test]
    fn two_different_inserts_at_the_same_point_conflict() {
        let anc = text(&["a", "b"]);
        let local = text(&["a", "L", "b"]);
        let server = text(&["a", "S", "b"]);
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert!(m.has_conflict(), "{:?}", m.regions);
        assert!(m.merged_local.is_none() && m.merged_server.is_none());
    }

    #[test]
    fn the_same_insert_at_the_same_point_applies_once() {
        let anc = text(&["a", "b"]);
        let both = text(&["a", "N", "b"]);
        let m = three_way(anc.as_bytes(), both.as_bytes(), both.as_bytes());
        assert_eq!(m.regions[0].kind, RegionKind::BothSame);
        assert_eq!(m.merged_local.as_deref(), Some(both.as_bytes()));
    }

    #[test]
    fn an_insert_next_to_the_other_sides_edit_applies_both() {
        let anc = text(&["a", "b", "c"]);
        let local = text(&["a", "N", "b", "c"]);
        let server = text(&["a", "b", "S", "c"]);
        assert_converges(&anc, &local, &server, &["a", "N", "b", "S", "c"]);
    }

    #[test]
    fn a_delete_next_to_an_edit_applies_both() {
        let anc = text(&["a", "b", "c", "d"]);
        let local = text(&["a", "c", "d"]);
        let server = text(&["a", "b", "c", "S"]);
        assert_converges(&anc, &local, &server, &["a", "c", "S"]);
    }

    #[test]
    fn a_delete_inside_the_other_sides_rewritten_block_conflicts() {
        let anc = text(&["a", "b", "c", "d"]);
        let local = text(&["a", "c", "d"]); // 只删了 b：祖先 1..2
        let server = text(&["a", "B", "S", "d"]); // b、c 被整段重写：祖先 1..3
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert!(m.has_conflict(), "{:?}", m.regions);
        assert!(m.merged_local.is_none() && m.merged_server.is_none());
    }

    #[test]
    fn a_one_sided_change_lands_on_the_other_side() {
        let anc = text(&["a", "b"]);
        let same = anc.clone();
        let changed = text(&["a", "b", "new"]);
        assert_converges(&anc, &changed, &same, &["a", "b", "new"]);
        assert_converges(&anc, &same, &changed, &["a", "b", "new"]);
    }

    #[test]
    fn each_side_keeps_its_own_bom_and_line_endings() {
        let anc = windows_text(&["a", "b"], false);
        let local = text(&["a", "b"]); // 本地：LF、无 BOM
        let server = windows_text(&["a", "NEW", "b"], true); // 服务器：CRLF、带 BOM，且插了一行
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert!(!m.has_conflict(), "{:?}", m.regions);
        // 服务器侧没被顺手改行尾：它一个字都不用动
        assert_eq!(m.merged_server.as_deref(), Some(server.as_bytes()));
        // 本地侧拿到 NEW，但按自己的 LF 写；BOM 也还是自己的（无）
        assert_eq!(m.merged_local.as_deref(), Some(text(&["a", "NEW", "b"]).as_bytes()));
        assert!(content_equal(&m.merged_local.unwrap(), &m.merged_server.unwrap()));
    }

    #[test]
    fn bom_and_eol_only_differences_are_not_changes() {
        let unix = text(&["a", "b"]);
        let crlf_bom = windows_text(&["a", "b"], true);
        let m = three_way(unix.as_bytes(), unix.as_bytes(), crlf_bom.as_bytes());
        assert!(!m.changed(), "{:?}", m.regions);
        assert_eq!(m.merged_local.as_deref(), Some(unix.as_bytes()));
        assert_eq!(m.merged_server.as_deref(), Some(crlf_bom.as_bytes()));
        assert!(content_equal(unix.as_bytes(), crlf_bom.as_bytes()));
    }

    #[test]
    fn trailing_whitespace_and_final_newline_are_not_changes() {
        let anc = text(&["a", "b"]);
        let local = "a  \nb\n"; // 只有行尾空白
        let server = "a\nb"; // 只有末尾换行
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert!(!m.changed(), "{:?}", m.regions);
        assert_eq!(m.merged_local.as_deref(), Some(local.as_bytes()));
        assert_eq!(m.merged_server.as_deref(), Some(server.as_bytes()));
    }

    #[test]
    fn a_real_trailing_blank_line_is_a_change() {
        let anc = "a\n".to_owned();
        let blank = "a\n\n".to_owned();
        assert_converges(&anc, &blank, &anc, &["a", ""]);
        assert_converges(&anc, &anc, &blank, &["a", ""]);
    }

    #[test]
    fn a_missing_final_newline_is_repaired_when_more_follows() {
        let anc = text(&["a", "b"]);
        let local = "a\nb".to_owned(); // 本地把末尾换行删了
        let server = text(&["a", "b", "S"]);
        assert_converges(&anc, &local, &server, &["a", "b", "S"]);
        // 不能把 S 直接黏在 b 上
        let m = three_way(anc.as_bytes(), local.as_bytes(), server.as_bytes());
        assert_eq!(m.merged_local.as_deref(), Some(text(&["a", "b", "S"]).as_bytes()));
    }

    #[test]
    fn upstream_line_survives_a_stale_local_side() {
        // 本地还停在 r190，工作副本已 update 到 r200、上游改了最后一行。
        // 祖先定在真正的岔口 r190 时：两处改动各归各，上游那一行留得住
        let r190 = text(&["a", "b", "c", "d"]);
        let local = text(&["a", "B", "c", "d"]);
        let r200 = text(&["a", "b", "c", "D"]);
        assert_converges(&r190, &local, &r200, &["a", "B", "c", "D"]);

        // 对照：把 BASE（=r200）直接当祖先，本地「没跟着更新」就被读成「本地把 D 退回了 d」，
        // 双向合并的结果是把上游的提交抹掉——这就是本设计不拿 BASE 当祖先的理由
        let m = three_way(r200.as_bytes(), local.as_bytes(), r200.as_bytes());
        assert!(!m.regions.is_empty());
        assert!(m.regions.iter().all(|r| r.kind == RegionKind::LocalOnly), "{:?}", m.regions);
        assert_eq!(
            String::from_utf8(m.merged_server.unwrap()).unwrap(),
            text(&["a", "B", "c", "d"]),
            "祖先选错时，合并会反向覆盖上游的提交"
        );
    }

    /// 文件太大跑不动精确 LCS 时，若只有一侧相对基线改过，仍应按「可证明」逻辑整文件覆盖过去，
    /// 而不是整文件跳过——大文件不等于不能自动合并（方案 A）
    #[test]
    fn too_big_still_resolves_a_one_sided_change() {
        let (local, server, _backup) = tree("too_big_one_sided");
        let anc = many("a", 50);
        let server_bytes = format!("{anc}X\n");
        put(&local, "src/a.java", anc.as_bytes(), 400);
        put(&server, "src/a.java", server_bytes.as_bytes(), 300);
        let stub = Stub::new().base_at("src/a.java", anc.as_bytes());
        // cell_limit=1 强制 too_big：任何非空文件的行数之积都大于 1
        let limits = MergeLimits { lcs_cell_limit: 1, ..Default::default() };
        let plan = plan(&local, &server, "", "", "", &stub, &stub, &limits).unwrap();
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::OverwriteServerToLocal, "只有服务器改过 → 整文件覆盖到本地");
        assert_eq!(entry.write_local.as_ref().unwrap(), server_bytes.as_bytes());
    }

    /// 大文件且两侧都相对基线（在容差内）改过：没有任何一方是「可证明」的，必须留给人工，不能瞎覆盖
    #[test]
    fn too_big_with_edits_on_both_sides_stays_undecided() {
        let (local, server, _backup) = tree("too_big_both");
        let anc = many("a", 50);
        let local_bytes = format!("{anc}L\n");
        let server_bytes = format!("{anc}S\n");
        put(&local, "src/a.java", local_bytes.as_bytes(), 400);
        put(&server, "src/a.java", server_bytes.as_bytes(), 300);
        let stub = Stub::new().base_at("src/a.java", anc.as_bytes());
        let limits = MergeLimits { lcs_cell_limit: 1, ..Default::default() };
        let plan = plan(&local, &server, "", "", "", &stub, &stub, &limits).unwrap();
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::SkipUndecided);
        assert!(entry.note.contains("两侧都改过"), "note = {}", entry.note);
        assert!(entry.write_local.is_none() && entry.write_server.is_none());
    }

    /// 无论最后提交人是否相同，都不能绕过大文件的双侧修改保护。
    #[test]
    fn same_author_does_not_override_the_lcs_limit() {
        let (local, server, _backup) = tree("same_author_too_big");
        let anc = many("a", 50);
        let local_bytes = format!("{anc}L\n");
        let server_bytes = format!("{anc}S\n");
        put(&local, "src/a.java", local_bytes.as_bytes(), 400);
        put(&server, "src/a.java", server_bytes.as_bytes(), 300);
        // 两侧桩都报同一个提交人
        let server_stub = Stub::new().base_at("src/a.java", anc.as_bytes()).author("alice");
        let local_stub = Stub::new().base_at("src/a.java", anc.as_bytes()).author("alice");
        // cell_limit=1 强制 too_big：任何非空文件的行数之积都大于 1
        let limits = MergeLimits { lcs_cell_limit: 1, ..Default::default() };
        let plan_same = plan(&local, &server, "", "", "", &server_stub, &local_stub, &limits).unwrap();
        let entry = &plan_same.entries[0];
        assert_eq!(entry.action, Action::SkipUndecided, "作者相同也不能覆盖另一侧改动");
        assert!(entry.write_server.is_none() && entry.write_local.is_none());

        // 两侧提交人不同：仍然留给人工，不能瞎覆盖
        let other = Stub::new().base_at("src/a.java", anc.as_bytes()).author("bob");
        let plan_diff = plan(&local, &server, "", "", "", &server_stub, &other, &limits).unwrap();
        let entry = &plan_diff.entries[0];
        assert_eq!(entry.action, Action::SkipUndecided, "作者不同仍留给人工");
        assert!(entry.note.contains("两侧都改过"), "note = {}", entry.note);
        assert!(entry.write_local.is_none() && entry.write_server.is_none());
    }

    /// 相同提交人也可能有两份未提交改动；真实冲突仍需人工处理。
    #[test]
    fn rename_candidates_never_escape_as_delete_and_add_jobs() {
        for rename_local in [true, false] {
            let (local, server, backup, state) = synced_tree(if rename_local { "rename_local_guard" } else { "rename_server_guard" });
            let root = if rename_local { &local } else { &server };
            fs::rename(root.join("src/a.java"), root.join("src/New.java")).unwrap();
            for _ in 0..2 {
                let plan = state_plan(&local, &server, &state);
                assert_eq!(plan.entries.len(), 2);
                assert_eq!(plan.writing(), 0);
                assert!(plan.entries.iter().all(|e| e.note.contains("改名")));
                assert_eq!(apply(&plan, &backup, false).unwrap().written, 0);
            }
            assert!(root.join("src/New.java").exists());
            assert!(!root.join("src/a.java").exists());
        }
    }

    #[test]
    fn file_and_plan_memory_limits_produce_no_oversized_jobs() {
        let (local, server, _) = tree("memory_limits");
        put(&local, "src/a.java", b"changed content\n", 100);
        put(&server, "src/a.java", b"old content\n", 100);
        let stub = Stub::new().base_at("src/a.java", b"old content\n");
        for limits in [MergeLimits { max_file_bytes: 3, ..Default::default() },
            MergeLimits { max_plan_bytes: 3, ..Default::default() }] {
            let plan = plan(&local, &server, "", "", "", &stub, &stub, &limits).unwrap();
            assert!(plan.budget_hit);
            assert_eq!(plan.writing(), 0);
            assert!(plan.entries.iter().all(|e| e.observed_local.is_none() && e.observed_server.is_none()));
        }
        assert_eq!(read_limited(&local.join("src/a.java"), 16).unwrap().len(), 16);
        assert!(read_limited(&local.join("src/a.java"), 15).is_err());
        let newlines = vec![b'\n'; 500_000];
        assert!(three_way_limited(&newlines, &newlines, &newlines, usize::MAX).too_big);
    }

    #[test]
    fn same_author_does_not_resolve_a_real_conflict() {
        let (local, server, _backup) = tree("same_author_conflict");
        let anc = text(&["a", "b", "c"]);
        let local_bytes = text(&["a", "L", "c"]);
        let server_bytes = text(&["a", "S", "c"]);
        put(&local, "src/a.java", local_bytes.as_bytes(), 400);
        put(&server, "src/a.java", server_bytes.as_bytes(), 300);
        let server_stub = Stub::new().base_at("src/a.java", anc.as_bytes()).author("alice");
        let local_stub = Stub::new().base_at("src/a.java", anc.as_bytes()).author("alice");
        let plan = plan(&local, &server, "", "", "", &server_stub, &local_stub, &MergeLimits::default()).unwrap();
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::SkipConflict, "相同提交人不能作为冲突处理依据");
        assert!(entry.write_server.is_none() && entry.write_local.is_none());
    }

    /// 相同提交人不能作为缺失共同基线时的覆盖依据。
    #[test]
    fn same_author_cannot_supply_a_missing_common_base() {
        let (local, server, _backup) = tree("same_author_toofar");
        put(&local, "src/a.java", many("L", 40).as_bytes(), 400);
        put(&server, "src/a.java", many("U", 40).as_bytes(), 300);
        let stub = Stub::new()
            .base_at("src/a.java", many("U", 40).as_bytes())
            .rev("src/a.java", 7, many("M", 30).as_bytes())
            .author("wangzhanpeng");
        let local_stub = Stub::new().state("src/a.java", WcState::Unversioned).author("wangzhanpeng");
        let plan = plan(&local, &server, "", "", "", &stub, &local_stub, &MergeLimits::default()).unwrap();
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::SkipUndecided);
        assert!(entry.write_server.is_none() && entry.write_local.is_none());
    }

    /// 差异过多但两侧提交人不同：仍然留给人工，标签如实写「差异过多」而非误导的「无基线」
    #[test]
    fn different_author_too_far_still_undecided_but_labeled_as_too_far() {
        let (local, server, _backup) = tree("diff_author_toofar");
        put(&local, "src/a.java", many("L", 40).as_bytes(), 400);
        put(&server, "src/a.java", many("U", 40).as_bytes(), 300);
        let server_stub = Stub::new()
            .base_at("src/a.java", many("U", 40).as_bytes())
            .rev("src/a.java", 7, many("M", 30).as_bytes())
            .author("alice");
        let local_stub = Stub::new()
            .state("src/a.java", WcState::Unversioned)
            .author("bob");
        let plan = plan(&local, &server, "", "", "", &server_stub, &local_stub, &MergeLimits::default()).unwrap();
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::SkipUndecided, "作者不同仍留给人工");
        assert_eq!(entry.action_text(), "差异过多", "标签要如实反映「差异过多」");
        assert!(entry.write_local.is_none() && entry.write_server.is_none());
    }

    #[test]
    fn bag_distance_counts_lines_not_offsets() {
        let a = text(&["x", "y", "z"]);
        assert_eq!(bag_distance(a.as_bytes(), a.as_bytes()), 0);
        // 开头插一行只算一行，不按位置把整篇错位都记上
        let b = text(&["h", "x", "y", "z"]);
        assert_eq!(bag_distance(a.as_bytes(), b.as_bytes()), 1);
        // 换一行 = 一去一回
        assert_eq!(bag_distance(a.as_bytes(), text(&["x", "Y", "z"]).as_bytes()), 2);
        // 纯按行内容集合比：顺序换了也是 0
        assert_eq!(bag_distance(a.as_bytes(), text(&["z", "x", "y"]).as_bytes()), 0);
        // 行尾空白与 BOM / 行尾都不算距离
        assert_eq!(bag_distance(a.as_bytes(), text(&["x  ", "y", "z"]).as_bytes()), 0);
        assert_eq!(bag_distance(a.as_bytes(), windows_text(&["x", "y", "z"], true).as_bytes()), 0);
    }

    #[test]
    fn too_big_bails_out_without_claiming_a_merge() {
        let anc = text(&["a", "b", "c"]);
        let local = text(&["a", "L", "c"]);
        let server = text(&["a", "S", "c"]);
        let m = three_way_limited(anc.as_bytes(), local.as_bytes(), server.as_bytes(), 1);
        assert!(m.too_big);
        assert!(m.regions.is_empty());
        assert!(m.merged_local.is_none() && m.merged_server.is_none());
    }

    #[test]
    fn identical_sides_produce_no_regions() {
        let t = text(&["a", "b"]);
        let m = three_way(t.as_bytes(), t.as_bytes(), t.as_bytes());
        assert!(!m.changed());
        assert_eq!(m.merged_local.as_deref(), Some(t.as_bytes()));
        assert_eq!(m.merged_server.as_deref(), Some(t.as_bytes()));
    }

    // ------------------------------------------------------------ 白名单范围

    /// 那一格留空 = 内置的代码 / 文本清单：源码与配置参与，构建产物、图片、压缩包不参与
    #[test]
    fn the_empty_scope_is_the_builtin_code_and_text_list() {
        let s = scope("");
        for hit in ["src/main/Foo.java", "conf/app.properties", "a.xml", "docs/note.md", "x.sql"] {
            assert!(s.hits(hit), "{hit} 该在默认白名单里");
        }
        for miss in ["bin/tool.exe", "web/dist/app.png", "pkg.zip", "src/main/Foo.class", "a.iml"] {
            assert!(!s.hits(miss), "{miss} 不该被静默搬来搬去");
        }
    }

    /// 写了内容就只按写的走：默认清单不偷偷兜底，否则「这次只合压缩包」这句话做不到
    #[test]
    fn a_written_scope_replaces_the_default_list() {
        let s = scope("*.zip");
        assert!(s.hits("pkg.zip"), "裸写法自动补 `+`");
        assert!(!s.hits("Foo.java"), "写了白名单就不再加默认清单");
    }

    /// 缝合规则与屏蔽框同一套：多段裸路径自动锚定到根、`#` 与空行丢掉
    #[test]
    fn the_scope_is_stitched_by_the_same_rules_as_the_shield_box() {
        let s = scope("# 只合这两个模块\nsrc\\main\\\n\nHRP.Web\\build\\\n");
        assert!(s.hits("src/main/Foo.java"));
        assert!(s.hits("HRP.Web/build/classes/a.xml"), "根锚定的多段目录要命中");
        assert!(!s.hits("other/HRP.Web/build/a.xml"), "锚定根目录就不该顺手命中深层同名目录");
    }

    // ------------------------------------------------------------ 计划与落盘的助手

    use std::sync::atomic::AtomicUsize;

    /// 内存桩：BASE 与历史版本按 rel 喂进来，`plan` 因此不需要 svn.exe，也不需要网络。
    /// 调用次数一并记账——「本地与基线一致就只问一次」是要断言的
    #[derive(Default)]
    struct Stub {
        base: HashMap<String, Vec<u8>>,
        local_base: HashMap<String, Vec<u8>>,
        history: HashMap<String, Vec<(u64, Vec<u8>)>>,
        states: HashMap<String, WcState>,
        offline: bool,
        calls: AtomicUsize,
        /// 这一侧桩返回的「最后提交人」；`None` = 取不到
        author: Option<String>,
    }

    impl Stub {
        fn new() -> Self {
            Self::default()
        }

        fn base_at(mut self, rel: &str, bytes: &[u8]) -> Self {
            self.base.insert(rel.to_owned(), bytes.to_vec());
            self
        }

        /// 历史版本按「新 → 旧」追加，与 `svn log -r BASE:1` 给出的顺序一致
        fn rev(mut self, rel: &str, rev: u64, bytes: &[u8]) -> Self {
            self.history.entry(rel.to_owned()).or_default().push((rev, bytes.to_vec()));
            self
        }

        fn state(mut self, rel: &str, state: WcState) -> Self {
            self.states.insert(rel.to_owned(), state);
            self
        }

        fn offline(mut self) -> Self {
            self.offline = true;
            self
        }

        /// 保留作者元数据作为回归输入；算法不得据此授权覆盖。
        fn author(mut self, author: &str) -> Self {
            self.author = Some(author.to_owned());
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }

        fn tick(&self) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Ancestor for Stub {
        fn wc_state(&self, rel: &str, exists: bool) -> WcState {
            self.states
                .get(rel)
                .copied()
                .unwrap_or(if exists { WcState::Normal } else { WcState::Absent })
        }

        fn base(&self, rel: &str) -> Result<Option<Vec<u8>>, String> {
            self.tick();
            Ok(self.base.get(rel).cloned())
        }

        fn revs(&self, rel: &str, max: usize) -> Result<Vec<u64>, String> {
            self.tick();
            Ok(self
                .history
                .get(rel)
                .map(|h| h.iter().take(max).map(|(r, _)| *r).collect())
                .unwrap_or_default())
        }

        fn at_rev(&self, rel: &str, rev: u64) -> Result<Option<Vec<u8>>, String> {
            self.tick();
            if self.offline {
                return Err("svn: E170013: Unable to connect to a repository".to_owned());
            }
            Ok(self
                .history
                .get(rel)
                .and_then(|h| h.iter().find(|(r, _)| *r == rev).map(|(_, b)| b.clone())))
        }

    }

    /// 真临时目录：`plan` / `apply` 走的是文件系统，只有 svn 能被桩掉
    fn tree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("repo_manager_merge_{name}"));
        let _ = fs::remove_dir_all(&root);
        let (local, server, backup) = (root.join("local"), root.join("server"), root.join("backup"));
        for dir in [&local, &server, &backup] {
            fs::create_dir_all(dir).unwrap();
        }
        (local, server, backup)
    }

    fn put(root: &Path, rel: &str, bytes: &[u8], modified: u64) {
        let path = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(modified)))
            .unwrap();
    }

    fn slurp(root: &Path, rel: &str) -> Vec<u8> {
        fs::read(root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))).unwrap()
    }

    /// 只给白名单那一格的取值：等价于黑名单留空
    fn merge_plan(local: &Path, server: &Path, include: &str, anc: &Stub) -> Plan {
        merge_plan_scoped(local, server, include, "", anc)
    }

    fn state_plan(local: &Path, server: &Path, state: &Path) -> Plan {
        let server_stub = Stub::new();
        let local_stub = Stub::new().state("src/a.java", WcState::Unversioned)
            .state("src/New.java", WcState::Unversioned)
            .state("src/gone.java", WcState::Unversioned);
        plan_with_state(local, server, "", "", "", &server_stub, &local_stub,
            &MergeLimits::default(), Some(state)).unwrap()
    }

    fn synced_tree(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let (local, server, backup) = tree(name);
        let state = backup.parent().unwrap().join("sync/state.json");
        put(&local, "src/a.java", b"a\nb\nc\n", 100);
        put(&server, "src/a.java", b"a\nb\nc\n", 100);
        assert!(state_plan(&local, &server, &state).entries.is_empty());
        (local, server, backup, state)
    }

    fn merge_plan_scoped(local: &Path, server: &Path, include: &str, exclude: &str, anc: &Stub) -> Plan {
        let mut local_anc = Stub { base: anc.base.clone(), states: anc.states.clone(), ..Default::default() };
        local_anc.base.extend(anc.local_base.clone());
        plan(local, server, "", include, exclude, anc, &local_anc, &MergeLimits::default()).unwrap()
    }

    /// n 行彼此互不相同的文本，用来把距离推到容差之外
    fn many(prefix: &str, n: usize) -> String {
        let rows: Vec<String> = (0..n).map(|i| format!("{prefix}{i}")).collect();
        text(&rows.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// 「祖先 r100 → 上游提交到 r200（= BASE = 工作副本现状）→ 本地停在 r100 上又加了自己的行」：
    /// 定岔与「不许覆盖别人提交」这两件事都靠这个场景验
    fn stale_local() -> (Stub, String, String, String) {
        let anc = text(&["a", "b", "c"]);
        let upstream = text(&["a", "b", "c", "S"]);
        let mine = text(&["a", "L", "b", "c"]);
        let stub = Stub::new()
            .base_at("src/a.java", upstream.as_bytes())
            .rev("src/a.java", 200, upstream.as_bytes())
            .rev("src/a.java", 100, anc.as_bytes());
        let mut stub = stub;
        stub.local_base.insert("src/a.java".into(), anc.as_bytes().to_vec());
        (stub, anc, upstream, mine)
    }

    // ---------------------------------------------------------------- 定岔

    #[test]
    fn the_stale_local_side_chooses_the_branch_point_not_the_base() {
        let (local, server, _backup) = tree("stale");
        let (stub, _, upstream, mine) = stale_local();
        put(&server, "src/a.java", upstream.as_bytes(), 300);
        put(&local, "src/a.java", mine.as_bytes(), 400);
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::MergeBoth);
        assert_eq!(entry.ancestor, AncInfo::Rev(100), "本地停在 r100 上，祖先就该是 r100 而不是 BASE");
        let want = ["a", "L", "b", "c", "S"];
        assert_eq!(keys(entry.write_local.as_ref().unwrap()), want, "本地侧要拿到上游那行");
        assert_eq!(keys(entry.write_server.as_ref().unwrap()), want, "服务器侧要拿到本地那行");
    }

    /// 本地与基线一字不差时不该再走网络：一次 `cat -r BASE` 就收
    #[test]
    fn a_base_that_matches_local_needs_no_history_walk() {
        let (local, server, _backup) = tree("exact_base");
        let base = text(&["a", "b"]);
        put(&local, "src/a.java", base.as_bytes(), 100);
        put(&server, "src/a.java", text(&["a", "b", "S"]).as_bytes(), 300);
        let stub = Stub::new().base_at("src/a.java", base.as_bytes()).rev("src/a.java", 9, base.as_bytes());
        let plan = merge_plan(&local, &server, "", &stub);
        assert_eq!(stub.calls(), 1, "只问了 BASE");
        assert_eq!(plan.entries[0].ancestor, AncInfo::Base);
        assert_eq!(plan.entries[0].action, Action::MergeBoth);
    }

    /// 日常最常见的一种：本地改了自己的行、上游这段时间没碰过这个文件。
    /// 这时 BASE 就是祖先，判定不能因为「没有比 BASE 更接近的历史版」而罢工
    #[test]
    fn local_edits_with_quiet_upstream_still_take_the_base() {
        let (local, server, _backup) = tree("quiet");
        let anc = text(&["a", "b", "c"]);
        put(&server, "src/a.java", anc.as_bytes(), 300);
        put(&local, "src/a.java", text(&["a", "L", "b", "c"]).as_bytes(), 400);
        let stub = Stub::new().base_at("src/a.java", anc.as_bytes());
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.ancestor, AncInfo::Base);
        assert_eq!(entry.action, Action::MergeBoth);
        assert!(entry.write_local.is_none(), "本地侧本来就是对的，不该动它");
        assert_eq!(keys(entry.write_server.as_ref().unwrap()), ["a", "L", "b", "c"]);
    }

    #[test]
    fn no_base_is_never_treated_as_an_empty_ancestor() {
        let (local, server, _backup) = tree("nobase");
        put(&local, "src/a.java", text(&["a", "L"]).as_bytes(), 400);
        put(&server, "src/a.java", text(&["a", "b"]).as_bytes(), 300);
        let stub = Stub::new();
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::SkipUndecided);
        assert!(matches!(entry.ancestor, AncInfo::Unknown(Uncertain::NoBase(_))));
        assert!(entry.write_local.is_none() && entry.write_server.is_none());
    }

    /// 定岔途中断线，就不许退回来拿 BASE 兜底：那点差距分不清是本地改动还是没跟着更新
    #[test]
    fn an_offline_walk_reports_offline_instead_of_guessing_the_base() {
        let (local, server, _backup) = tree("offline");
        let (stub, anc, upstream, mine) = stale_local();
        put(&local, "src/a.java", mine.as_bytes(), 400);
        put(&server, "src/a.java", upstream.as_bytes(), 300);
        let stub = stub.offline();
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::SkipUndecided);
        assert!(entry.note.contains("连不上服务器"), "note = {}", entry.note);
        assert_eq!(anc, text(&["a", "b", "c"]));
    }

    /// 上游整篇重写时距离收不拢：报告必须点名最接近的版本与具体行数，否则像 bug
    #[test]
    fn too_far_names_the_closest_version_and_its_distance() {
        let (local, server, _backup) = tree("toofar");
        put(&local, "src/a.java", many("L", 40).as_bytes(), 400);
        put(&server, "src/a.java", many("U", 40).as_bytes(), 300);
        let stub = Stub::new()
            .base_at("src/a.java", many("U", 40).as_bytes())
            .rev("src/a.java", 7, many("M", 30).as_bytes());
        let local_stub = Stub::new().state("src/a.java", WcState::Unversioned);
        let plan = plan(&local, &server, "", "", "", &stub, &local_stub, &MergeLimits::default()).unwrap();
        assert_eq!(
            plan.entries[0].ancestor,
            AncInfo::Unknown(Uncertain::TooFar { candidate: 7, distance: 70, threshold: 24 }),
            "r7 的 30 行与本地的 40 行互不相同，对称差就是 70"
        );
        assert_eq!(plan.entries[0].action, Action::SkipUndecided);
    }

    #[test]
    fn the_svn_call_budget_stops_without_claiming_the_scan_finished() {
        let (local, server, _backup) = tree("budget");
        let base = text(&["a", "b"]);
        for name in ["x.java", "y.java", "z.java"] {
            put(&local, &format!("src/{name}"), base.as_bytes(), 100);
            put(&server, &format!("src/{name}"), text(&["a", "b", "S"]).as_bytes(), 300);
        }
        let stub = Stub::new()
            .base_at("src/x.java", base.as_bytes())
            .base_at("src/y.java", base.as_bytes())
            .base_at("src/z.java", base.as_bytes());
        let limits = MergeLimits { svn_calls: 2, ..Default::default() };
        let plan = plan(&local, &server, "", "", "", &stub, &stub, &limits).unwrap();
        assert!(plan.budget_hit);
        assert_eq!(plan.untried, 1, "第三条撞线没判");
        assert_eq!(plan.entries.len(), 2);
    }

    // ---------------------------------------------------------- 白名单与整文件档

    #[test]
    fn a_file_outside_the_whitelist_is_counted_not_silently_dropped() {
        let (local, server, _backup) = tree("whitelist");
        put(&local, "pkg/tool.zip", b"\x00\x01zip-local".as_slice(), 400);
        put(&server, "pkg/tool.zip", b"\x00\x09zip-server".as_slice(), 300);
        let stub = Stub::new();
        let plan = merge_plan(&local, &server, "", &stub);
        assert_eq!(plan.entries[0].action, Action::SkipNotWhitelisted);
        assert_eq!(plan.outside_whitelist, 1);
        assert_eq!(stub.calls(), 0, "白名单外连 svn 都不该问");
    }

    /// 他把 `*.zip` 显式写进白名单，才允许整文件覆盖；且只有「本地与基线一字不差」这方向可证明
    #[test]
    fn an_added_whitelist_entry_allows_a_whole_file_copy_down() {
        let (local, server, _backup) = tree("zip_copy");
        let base = b"\x00\x01same";
        put(&local, "pkg/tool.zip", base.as_slice(), 400);
        put(&server, "pkg/tool.zip", b"\x00\x09new-from-upstream".as_slice(), 300);
        let stub = Stub::new().base_at("pkg/tool.zip", base);
        let plan = merge_plan(&local, &server, "*.zip", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::OverwriteServerToLocal);
        assert_eq!(entry.write_local.as_ref().unwrap(), b"\x00\x09new-from-upstream");
        assert!(entry.write_server.is_none(), "服务器端本来就是基线，不该被反向写");
    }

    #[test]
    fn a_binary_both_sides_changed_is_left_alone() {
        let (local, server, _backup) = tree("zip_both");
        put(&local, "pkg/tool.zip", b"\x00local-edited".as_slice(), 400);
        put(&server, "pkg/tool.zip", b"\x00server-edited".as_slice(), 300);
        let stub = Stub::new().base_at("pkg/tool.zip", b"\x00ancestor".as_slice());
        let plan = merge_plan(&local, &server, "*.zip", &stub);
        assert_eq!(plan.entries[0].action, Action::SkipBinary);
        assert!(plan.entries[0].write_local.is_none() && plan.entries[0].write_server.is_none());
    }

    // ------------------------------------------------------------------ 黑名单

    /// 两格同时命中按黑名单算：这一条要挡得住白名单，否则「明确写了不要」敌不过一句顺手加进来。
    /// `src/c.zip` 是双料命中——它不在 `*.java` 白名单里，又落在黑名单里，
    /// 只有它记成「黑名单内」才说明这条规则真的咬住了，而不是被白名单先一步推给别的原因
    #[test]
    fn the_blacklist_wins_over_the_whitelist() {
        let (local, server, _backup) = tree("blacklist");
        let base = text(&["a", "b", "c"]);
        for name in ["a.java", "b.java"] {
            put(&local, &format!("src/{name}"), text(&["a", "L", "c"]).as_bytes(), 400);
            put(&server, &format!("src/{name}"), base.as_bytes(), 300);
        }
        put(&local, "src/c.zip", b"\x00\x01zip-local".as_slice(), 500);
        put(&server, "src/c.zip", b"\x00\x09zip-server".as_slice(), 300);
        let stub = Stub::new()
            .base_at("src/a.java", base.as_bytes())
            .base_at("src/b.java", base.as_bytes());
        let plan = merge_plan_scoped(&local, &server, "*.java", "b.java\nc.zip", &stub);
        let kept = &plan.entries[0];
        assert_eq!(kept.rel, "src/a.java");
        assert_eq!(kept.action, Action::MergeBoth);
        assert!(kept.write_server.is_some() && kept.write_local.is_none());
        let blocked = &plan.entries[1];
        assert_eq!(blocked.action, Action::SkipBlacklisted, "黑名单内不该有任何写入");
        assert!(blocked.write_local.is_none() && blocked.write_server.is_none());
        assert_eq!(
            plan.entries[2].action,
            Action::SkipBlacklisted,
            "白名单外又黑名单内的，要按黑名单记"
        );
        assert_eq!(plan.blacklisted, 2);
        assert_eq!(plan.outside_whitelist, 0, "挡掉的原因记在黑名单那一格，不算白名单外");
        assert_eq!(plan.writing(), 1, "写入的只该是没被挡的那一条");
        // 一条都不放过：全列进黑名单后连 svn 都不该问一次（定岔要几次调用是另一回事，
        // 这里比的是「问都没问」，所以换个桩把三条都挡掉再数）
        let quiet = Stub::new()
            .base_at("src/a.java", base.as_bytes())
            .base_at("src/b.java", base.as_bytes());
        let all_blocked = merge_plan_scoped(&local, &server, "*.java", "*.java\n*.zip", &quiet);
        assert_eq!(all_blocked.blacklisted, 3);
        assert_eq!(quiet.calls(), 0, "黑名单内的条目一次 svn 都不该问");
    }

    /// 目录写法吃的是整棵子树：`target\` 要挡住深层里的每一个文件，与提交屏蔽那格同一套命中口径
    #[test]
    fn a_blacklisted_directory_blocks_its_whole_subtree() {
        let (local, server, _backup) = tree("blacklist_dir");
        let base = text(&["x", "y"]);
        for rel in ["src/keep.java", "pkg/target/deep/gen.java", "other/m.java"] {
            put(&local, rel, text(&["x", "L"]).as_bytes(), 400);
            put(&server, rel, base.as_bytes(), 300);
        }
        let stub = Stub::new()
            .base_at("src/keep.java", base.as_bytes())
            .base_at("pkg/target/deep/gen.java", base.as_bytes())
            .base_at("other/m.java", base.as_bytes());
        let plan = merge_plan_scoped(&local, &server, "*.java", "target\\", &stub);
        let actions = plan
            .entries
            .iter()
            .map(|e| (e.rel.clone(), e.action))
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![
                ("other/m.java".to_owned(), Action::MergeBoth),
                ("pkg/target/deep/gen.java".to_owned(), Action::SkipBlacklisted),
                ("src/keep.java".to_owned(), Action::MergeBoth),
            ]
        );
        assert_eq!(plan.writing(), 2);
    }

    /// 留空就是这条规则不存在；而不管两格怎么写，差异核对的条数都不该跟着动——
    /// 黑名单只收窄合并范围，不改「本地差异 N 项」那个口径
    #[test]
    fn an_empty_blacklist_blocks_nothing_and_does_not_move_the_diff_count() {
        assert!(blacklist("").is_none());
        assert!(blacklist("# 构建产物\n\n   \n").is_none(), "只有注释与空行等于没写");
        let (local, server, _backup) = tree("blacklist_empty");
        let base = text(&["a", "b", "c"]);
        put(&local, "src/a.java", text(&["a", "L", "c"]).as_bytes(), 400);
        put(&server, "src/a.java", base.as_bytes(), 300);
        put(&local, "src/b.java", text(&["a", "M", "c"]).as_bytes(), 400);
        put(&server, "src/b.java", base.as_bytes(), 300);
        let stub = Stub::new()
            .base_at("src/a.java", base.as_bytes())
            .base_at("src/b.java", base.as_bytes());
        let open = merge_plan_scoped(&local, &server, "*.java", "", &stub);
        assert_eq!(open.blacklisted, 0);
        assert_eq!(open.writing(), 2);
        let blocked = merge_plan_scoped(&local, &server, "*.java", "b.java", &stub);
        assert_eq!(blocked.checked, open.checked, "挡一条不该改变核对过的条数");
        assert_eq!(blocked.out_of_scope(), 1);
        assert_eq!(blocked.writing(), 1);
    }

    // ------------------------------------------------------------ 工作副本状态

    /// 计划删除的文件拷回去等于把这次删除撤销，哪怕只在服务器端有也一样
    #[test]
    fn a_scheduled_deletion_is_not_resurrected() {
        let (local, server, _backup) = tree("deleted");
        put(&server, "src/gone.java", text(&["g"]).as_bytes(), 300);
        let stub = Stub::new().state("src/gone.java", WcState::Deleted);
        let plan = merge_plan(&local, &server, "", &stub);
        assert_eq!(plan.entries[0].action, Action::SkipUndecided);
        assert!(plan.entries[0].note.contains("已计划删除"), "note = {}", plan.entries[0].note);
        assert!(plan.entries[0].write_local.is_none());
    }

    #[test]
    fn a_conflicted_working_copy_file_is_not_touched() {
        let (local, server, _backup) = tree("conflict_wc");
        put(&local, "src/a.java", text(&["a", "L"]).as_bytes(), 400);
        put(&server, "src/a.java", text(&["a", "b"]).as_bytes(), 300);
        let stub = Stub::new().state("src/a.java", WcState::Conflict);
        let plan = merge_plan(&local, &server, "", &stub);
        assert_eq!(plan.entries[0].action, Action::SkipUndecided);
        assert_eq!(stub.calls(), 0, "状态不对就不该去问祖先");
    }

    #[test]
    fn a_local_only_file_copies_in_with_its_own_spelling() {
        let (local, server, _backup) = tree("only_local");
        put(&local, "src/New.java", text(&["n"]).as_bytes(), 400);
        // 本地新加、还没进版本控制：未版本化 → 不是「服务器删了它」，该拷上去
        let stub = Stub::new().state("src/New.java", WcState::Unversioned);
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::CopyLocalToServer);
        assert_eq!(entry.server_rel, "src/New.java", "目标 rel 要跟着本地写法走");
        assert_eq!(entry.ancestor, AncInfo::NotNeeded);
        assert_eq!(entry.write_server.as_ref().unwrap(), text(&["n"]).as_bytes());
    }

    /// 反向：工作副本里新增、本地开发侧还没有的文件要拷出去。单侧新增不问祖先，
    /// 只看工作副本状态；这一向漏了就是「合并跑完，本地侧还是少几个新文件」
    #[test]
    fn a_server_only_file_copies_out_to_the_local_side() {
        let (local, server, backup) = tree("only_server");
        put(&server, "src/views/New.vue", text(&["v"]).as_bytes(), 300);
        let stub = Stub::new();
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::CopyServerToLocal);
        assert_eq!(entry.local_rel, "src/views/New.vue");
        assert_eq!(entry.ancestor, AncInfo::NotNeeded);
        assert_eq!(entry.write_local.as_ref().unwrap(), text(&["v"]).as_bytes());
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 1, "failed = {:?}", done.failed);
        assert_eq!(slurp(&local, "src/views/New.vue"), text(&["v"]).as_bytes());
    }

    /// 服务器端有人删了文件、本地还留着版本化的那一份：合并要把本地这份也删掉（删前先备份、
    /// 还能还原），而不是当成「本地新增」把它拷回服务器去复活。
    #[test]
    fn a_server_side_deletion_deletes_the_local_copy() {
        let (local, server, backup) = tree("server_deleted");
        put(&local, "src/gone.java", text(&["g"]).as_bytes(), 400);
        let state_path = backup.parent().unwrap().join("sync/state.json");
        put(&server, "src/gone.java", text(&["g"]).as_bytes(), 300);
        state_plan(&local, &server, &state_path);
        fs::remove_file(server.join("src/gone.java")).unwrap();
        let plan = state_plan(&local, &server, &state_path);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::DeleteLocal);
        assert!(entry.delete_local);
        assert_eq!(entry.action_text(), "服务器已删除");
        assert!(entry.write_server.is_none(), "不该把它拷回服务器");
        assert_eq!(plan.writing(), 1, "删除也算一次写入");
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 1, "failed = {:?}", done.failed);
        assert!(!local.join("src/gone.java").exists(), "本地副本该被删掉");
        let back = restore(done.backup.as_ref().unwrap()).unwrap();
        assert_eq!(back.restored, 1, "failed = {:?}", back.failed);
        assert_eq!(slurp(&local, "src/gone.java"), text(&["g"]).as_bytes());
    }

    /// 服务器工作副本里这个文件已计划删除 / 已丢失（状态表里看得见）：本地这份同样跟着删
    #[test]
    fn a_server_pending_deletion_also_removes_the_local_copy() {
        let (local, server, _backup) = tree("server_deleted_pending");
        put(&local, "src/gone.java", text(&["g"]).as_bytes(), 400);
        let stub = Stub::new().state("src/gone.java", WcState::Deleted).base_at("src/gone.java", text(&["g"]).as_bytes());
        let plan = merge_plan(&local, &server, "", &stub);
        assert_eq!(plan.entries[0].action, Action::DeleteLocal);
        assert!(plan.entries[0].note.contains("服务器端已删除"), "note = {}", plan.entries[0].note);
    }

    #[test]
    fn uncertain_single_side_states_do_not_resurrect_or_delete_files() {
        let (local, server, _backup) = tree("uncertain_single");
        put(&local, "src/a.java", b"local contents", 400);
        for state in [WcState::Conflict, WcState::Other] {
            let stub = Stub::new().state("src/a.java", state);
            let plan = merge_plan(&local, &server, "", &stub);
            assert_eq!(plan.entries[0].action, Action::SkipUndecided);
            assert_eq!(plan.writing(), 0);
            assert!(!plan.entries[0].delete_local);
            assert!(plan.entries[0].write_server.is_none());
        }
    }

    /// 时间预算只管要问祖先的那批：单侧增删读一个文件就能定、一次 svn 都不问，
    /// 让它跟着撞线就是「合并跑完本地侧还是少几个新文件」——按字母序排在后面的那几个
    #[test]
    fn a_blown_time_budget_still_copies_the_single_side_files() {
        let (local, server, _backup) = tree("budget_singles");
        put(&server, "src/z/NewOnServer.vue", text(&["v"]).as_bytes(), 300);
        put(&local, "src/z/NewOnLocal.java", text(&["n"]).as_bytes(), 400);
        // 本地新加的未版本化文件：不是删除，照旧拷上去
        let stub = Stub::new().state("src/z/NewOnLocal.java", WcState::Unversioned);
        let limits = MergeLimits { max_time: Duration::ZERO, ..Default::default() };
        let plan = plan(&local, &server, "", "", "", &stub, &stub, &limits).unwrap();
        let action_of = |rel: &str| plan.entries.iter().find(|e| e.rel == rel).map(|e| e.action);
        assert_eq!(action_of("src/z/newonserver.vue"), Some(Action::CopyServerToLocal));
        assert_eq!(action_of("src/z/newonlocal.java"), Some(Action::CopyLocalToServer));
        assert_eq!(plan.untried, 0, "单侧这两条不该算进未判定");
    }

    /// 同一对文件只差大小写时各写各的路径，不能谁后写谁赢
    #[test]
    fn each_side_writes_to_its_own_spelling() {
        let (local, server, backup) = tree("case_only");
        let (stub, _, upstream, mine) = stale_local();
        put(&server, "src/a.java", upstream.as_bytes(), 300);
        put(&local, "Src/A.java", mine.as_bytes(), 400);
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!((entry.local_rel.as_str(), entry.server_rel.as_str()), ("Src/A.java", "src/a.java"));
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 2);
        assert_eq!(keys(&slurp(&local, "Src/A.java")), ["a", "L", "b", "c", "S"]);
        assert_eq!(keys(&slurp(&server, "src/a.java")), ["a", "L", "b", "c", "S"]);
        assert!(content_equal(&slurp(&local, "Src/A.java"), &slurp(&server, "src/a.java")));
    }

    // ---------------------------------------------------------------- 落盘与还原

    #[test]
    fn apply_backs_up_both_sides_then_restore_puts_the_originals_back() {
        let (local, server, backup) = tree("round_trip");
        let (stub, _, upstream, mine) = stale_local();
        put(&server, "src/a.java", upstream.as_bytes(), 300);
        put(&local, "src/a.java", mine.as_bytes(), 400);
        let plan = merge_plan(&local, &server, "", &stub);
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 2);
        assert!(done.failed.is_empty(), "failed = {:?}", done.failed);
        assert_eq!(fs::read(done.backup.as_ref().unwrap().join("local").join("src/a.java")).unwrap(), mine.as_bytes());
        assert_eq!(fs::read(done.backup.as_ref().unwrap().join("server").join("src/a.java")).unwrap(), upstream.as_bytes());
        assert_ne!(slurp(&local, "src/a.java"), mine.as_bytes(), "落盘要真的改了文件");
        let back = restore(done.backup.as_ref().unwrap()).unwrap();
        assert_eq!(back.restored, 2);
        assert!(back.failed.is_empty(), "failed = {:?}", back.failed);
        assert_eq!(slurp(&local, "src/a.java"), mine.as_bytes());
        assert_eq!(slurp(&server, "src/a.java"), upstream.as_bytes());
    }

    /// 预览是建议不是锁：文件在预览之后被改过就不写，免得把他刚存的东西合掉
    #[test]
    fn a_file_changed_after_the_preview_is_skipped() {
        let (local, server, backup) = tree("stale_preview");
        let (stub, _, upstream, mine) = stale_local();
        put(&server, "src/a.java", upstream.as_bytes(), 300);
        put(&local, "src/a.java", mine.as_bytes(), 400);
        let plan = merge_plan(&local, &server, "", &stub);
        put(&server, "src/a.java", text(&["a", "b", "c", "刚存的"]).as_bytes(), 999);
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 0);
        assert!(done.failed[0].contains("预览之后文件被改过"), "failed = {:?}", done.failed);
        assert_eq!(slurp(&local, "src/a.java"), mine.as_bytes(), "另一侧也不该被单方面写过去");
    }

    /// 一个目标写失败不该拖累别的文件，已写出去的那份还能还原
    #[test]
    fn one_failing_target_leaves_the_others_applied_and_restorable() {
        let (local, server, backup) = tree("partial");
        let (stub, anc, upstream, mine) = stale_local();
        put(&server, "src/a.java", upstream.as_bytes(), 300);
        put(&local, "src/a.java", mine.as_bytes(), 400);
        put(&server, "src/b.java", upstream.as_bytes(), 300);
        put(&local, "src/b.java", text(&["a", "M", "b", "c"]).as_bytes(), 400);
        let stub = stub
            .base_at("src/b.java", upstream.as_bytes())
            .rev("src/b.java", 200, upstream.as_bytes())
            .rev("src/b.java", 100, anc.as_bytes());
        let mut stub = stub;
        stub.local_base.insert("src/b.java".into(), anc.as_bytes().to_vec());
        let plan = merge_plan(&local, &server, "", &stub);
        assert_eq!(plan.writing(), 2);
        let blocked = local.join("src").join("b.java");
        let set_readonly = |on: bool| {
            let mut perm = fs::metadata(&blocked).unwrap().permissions();
            perm.set_readonly(on);
            fs::set_permissions(&blocked, perm).unwrap();
        };
        set_readonly(true);
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 2, "a.java 两边都该写上");
        assert_eq!(done.failed.len(), 1, "failed = {:?}", done.failed);
        assert!(done.failed[0].contains("只读"), "failed = {:?}", done.failed);
        assert_eq!(keys(&slurp(&local, "src/b.java")), ["a", "M", "b", "c"], "只读的那份没动");
        set_readonly(false);
        let back = restore(done.backup.as_ref().unwrap()).unwrap();
        assert_eq!(back.restored, 2, "只读文件所在的那一对没有写入，不占还原记录");
        assert!(back.failed.is_empty(), "failed = {:?}", back.failed);
        assert_eq!(slurp(&local, "src/a.java"), mine.as_bytes());
        assert_eq!(slurp(&server, "src/a.java"), upstream.as_bytes());
    }

    /// 拷进来的新文件没有旧内容可备份，还原靠「还是不是我们写的那份」决定删不删
    #[test]
    fn restore_removes_a_file_that_was_copied_in() {
        let (local, server, backup) = tree("copy_back");
        put(&local, "src/New.java", text(&["n"]).as_bytes(), 400);
        let stub = Stub::new().state("src/New.java", WcState::Unversioned);
        let plan = merge_plan(&local, &server, "", &stub);
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 1);
        assert!(server.join("src").join("New.java").is_file());
        let back = restore(done.backup.as_ref().unwrap()).unwrap();
        assert_eq!(back.removed, 1);
        assert!(!server.join("src").join("New.java").exists());
    }

    /// 「我已确认无误，直接合并」：一个字节都不备份，按清单照样写完。没有还原点是这条路的代价，
    /// 所以 `Applied.backup` 必须是 `None`、备份根目录必须还是空的；不备份也不放松核对，
    /// 拿同一份清单重跑一次，指纹重查仍然要一项都不写
    #[test]
    fn a_direct_merge_writes_everything_and_leaves_no_undo_point() {
        let (local, server, backup) = tree("direct_merge");
        let (stub, _, upstream, mine) = stale_local();
        put(&server, "src/a.java", upstream.as_bytes(), 300);
        put(&local, "src/a.java", mine.as_bytes(), 400);
        let plan = merge_plan(&local, &server, "", &stub);
        let done = apply(&plan, &backup, false).unwrap();
        assert_eq!(done.written, 2, "failed = {:?}", done.failed);
        assert!(done.backup.is_none(), "直接合并不该报出一个备份目录");
        assert!(backups(&backup).is_empty(), "备份根目录里不该多出一次会话");
        assert_eq!(fs::read_dir(&backup).unwrap().count(), 0, "连会话目录都不该建出来");
        assert_eq!(keys(&slurp(&local, "src/a.java")), ["a", "L", "b", "c", "S"]);
        assert_eq!(keys(&slurp(&server, "src/a.java")), ["a", "L", "b", "c", "S"]);
        let again = apply(&plan, &backup, false).unwrap();
        assert_eq!(again.written, 0, "文件已经被上一次写过，这一趟一项都不该再动");
        assert!(!again.failed.is_empty(), "不备份这条路上预览过期的重查同样要生效");
    }

    /// 崩在一次 apply 中间留下的临时文件不该被算成「只在本地开发侧」
    #[test]
    fn crash_leftovers_are_not_counted_as_a_difference() {
        let (local, server, _backup) = tree("leftover");
        put(&local, ".repomanager-merge-tmp-4242-0", b"half written".as_slice(), 400);
        put(&local, "src/a.java", text(&["a"]).as_bytes(), 300);
        put(&server, "src/a.java", text(&["a"]).as_bytes(), 300);
        let stub = Stub::new();
        let plan = merge_plan(&local, &server, "", &stub);
        assert!(plan.entries.is_empty(), "entries = {:?}", plan.entries.iter().map(|e| &e.rel).collect::<Vec<_>>());
    }

    #[test]
    fn only_the_newest_backups_are_kept() {
        let (_local, _server, backup) = tree("prune");
        for i in 0..7 {
            let dir = backup.join(format!("{BACKUP_PREFIX}2026010{i}-000000"));
            fs::create_dir_all(&dir).unwrap();
            append_journal(&dir, &JournalLine { kind: "finished".into(), ..Default::default() }).unwrap();
        }
        prune(&backup, BACKUPS_KEPT);
        assert_eq!(backups(&backup).len(), BACKUPS_KEPT);
        assert!(backup.join(format!("{BACKUP_PREFIX}20260106-000000")).is_dir());
        assert!(!backup.join(format!("{BACKUP_PREFIX}20260100-000000")).is_dir());
    }

    #[test]
    fn a_local_deletion_propagates_and_can_be_restored() {
        let (local, server, backup, state) = synced_tree("local_delete_roundtrip");
        fs::remove_file(local.join("src/a.java")).unwrap();
        let plan = state_plan(&local, &server, &state);
        assert_eq!(plan.entries[0].action, Action::DeleteServer);
        assert_eq!(plan.deletes(), 1);
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 1, "{:?}", done.failed);
        assert!(!server.join("src/a.java").exists());
        let back = restore(done.backup.as_ref().unwrap()).unwrap();
        assert!(back.failed.is_empty(), "{:?}", back.failed);
        assert_eq!(slurp(&server, "src/a.java"), b"a\nb\nc\n");
    }

    #[test]
    fn deletion_against_modification_is_a_conflict_in_both_directions() {
        for delete_local in [true, false] {
            let (local, server, backup, state) = synced_tree(if delete_local { "delete_modify_l" } else { "delete_modify_s" });
            let (deleted, edited) = if delete_local { (&local, &server) } else { (&server, &local) };
            fs::remove_file(deleted.join("src/a.java")).unwrap();
            put(edited, "src/a.java", b"a\nB\nc\n", 100);
            let plan = state_plan(&local, &server, &state);
            assert_eq!(plan.entries[0].action, Action::SkipConflict);
            assert_eq!(plan.writing(), 0);
            let done = apply(&plan, &backup, false).unwrap();
            assert_eq!(done.written, 0);
            assert_eq!(slurp(edited, "src/a.java"), b"a\nB\nc\n");
            assert!(!deleted.join("src/a.java").exists());
        }
    }

    #[test]
    fn pending_deletion_without_an_unchanged_local_baseline_is_not_applied() {
        let (local, server, _) = tree("pending_delete_modified");
        put(&local, "src/a.java", b"edited\n", 100);
        let server_stub = Stub::new().state("src/a.java", WcState::Deleted);
        let local_stub = Stub::new().base_at("src/a.java", b"original\n");
        let plan = plan(&local, &server, "", "", "", &server_stub, &local_stub, &MergeLimits::default()).unwrap();
        assert_eq!(plan.entries[0].action, Action::SkipConflict);
        assert!(!plan.entries[0].delete_local);
    }

    #[test]
    fn a_changed_or_deleted_source_invalidates_a_one_way_preview() {
        for remove in [false, true] {
            let (local, server, backup, state) = synced_tree(if remove { "source_removed" } else { "source_changed" });
            put(&server, "src/a.java", b"a\nB\nc\n", 100);
            let plan = state_plan(&local, &server, &state);
            assert!(plan.entries[0].write_server.is_none());
            assert!(plan.entries[0].write_local.is_some());
            if remove { fs::remove_file(server.join("src/a.java")).unwrap(); }
            else { put(&server, "src/a.java", b"a\nX\nc\n", 100); }
            let done = apply(&plan, &backup, false).unwrap();
            assert_eq!(done.written, 0);
            assert_eq!(done.failed.len(), 1);
            assert_eq!(slurp(&local, "src/a.java"), b"a\nb\nc\n");
        }
    }

    #[test]
    fn a_recreated_source_cancels_a_previewed_deletion() {
        let (local, server, backup, state) = synced_tree("source_recreated");
        fs::remove_file(server.join("src/a.java")).unwrap();
        let plan = state_plan(&local, &server, &state);
        assert_eq!(plan.entries[0].action, Action::DeleteLocal);
        put(&server, "src/a.java", b"restored on server\n", 100);
        let done = apply(&plan, &backup, false).unwrap();
        assert_eq!(done.written, 0);
        assert_eq!(slurp(&local, "src/a.java"), b"a\nb\nc\n");
    }

    #[test]
    fn a_missing_server_directory_is_not_treated_as_a_confirmed_file_deletion() {
        let (local, server, backup, state) = synced_tree("source_directory_unavailable");
        fs::remove_file(server.join("src/a.java")).unwrap();
        let plan = state_plan(&local, &server, &state);
        assert_eq!(plan.entries[0].action, Action::DeleteLocal);
        fs::rename(&server, server.with_file_name("server-unavailable")).unwrap();
        assert!(apply(&plan, &backup, false).unwrap_err().contains("绑定目录已不可用"));
        assert_eq!(slurp(&local, "src/a.java"), b"a\nb\nc\n");
    }

    #[test]
    fn first_time_one_sided_files_are_not_guessed_as_new_even_after_refresh() {
        for on_local in [true, false] {
            let (local, server, _) = tree(if on_local { "unknown_initial_l" } else { "unknown_initial_s" });
            let state = local.parent().unwrap().join("sync/state.json");
            put(if on_local { &local } else { &server }, "src/a.java", b"existing unknown file\n", 100);
            for _ in 0..2 {
                let plan = state_plan(&local, &server, &state);
                assert_eq!(plan.writing(), 0);
                assert_eq!(plan.entries[0].action, Action::SkipUndecided);
            }
        }
    }

    #[test]
    fn a_proven_new_file_remains_new_across_repeated_previews() {
        let (local, server, backup, state) = synced_tree("new_file_refresh");
        put(&local, "src/New.java", b"new\n", 100);
        assert_eq!(state_plan(&local, &server, &state).entries[0].action, Action::CopyLocalToServer);
        let refreshed = state_plan(&local, &server, &state);
        assert_eq!(refreshed.entries[0].action, Action::CopyLocalToServer);
        assert_eq!(apply(&refreshed, &backup, false).unwrap().written, 1);
        assert_eq!(slurp(&server, "src/New.java"), b"new\n");
    }

    #[test]
    fn a_local_working_copy_conflict_also_prevents_merging() {
        let (local, server, _) = tree("local_conflict_wc");
        put(&local, "src/a.java", b"local conflict markers\n", 100);
        put(&server, "src/a.java", b"server\n", 100);
        let server_stub = Stub::new().base_at("src/a.java", b"base\n");
        let local_stub = Stub::new().state("src/a.java", WcState::Conflict);
        let plan = plan(&local, &server, "", "", "", &server_stub, &local_stub, &MergeLimits::default()).unwrap();
        assert_eq!(plan.writing(), 0);
        assert_eq!(plan.entries[0].action, Action::SkipUndecided);
        assert_eq!(server_stub.calls(), 0);
    }

    #[test]
    fn restore_preserves_same_size_edits_in_existing_and_new_files() {
        for fresh in [false, true] {
            let (local, server, backup, state) = synced_tree(if fresh { "restore_fresh_changed" } else { "restore_existing_changed" });
            let rel = if fresh { "src/New.java" } else { "src/a.java" };
            put(&local, rel, b"mine\n", 100);
            let plan = state_plan(&local, &server, &state);
            let done = apply(&plan, &backup, true).unwrap();
            assert_eq!(done.written, 1, "{:?}", done.failed);
            put(&server, rel, b"edit\n", 100);
            let back = restore(done.backup.as_ref().unwrap()).unwrap();
            assert_eq!(back.failed.len(), 1);
            assert_eq!(back.removed, 0);
            assert_eq!(slurp(&server, rel), b"edit\n");
        }
    }

    #[test]
    fn synced_files_merge_without_svn_and_the_next_sync_uses_the_new_baseline() {
        let (local, server, backup, state) = synced_tree("plain_directory_sync");
        put(&local, "src/a.java", b"a\nL\nb\nc\n", 100);
        put(&server, "src/a.java", b"a\nb\nc\nS\n", 100);
        let plan = state_plan(&local, &server, &state);
        assert_eq!(plan.entries[0].ancestor, AncInfo::Synced);
        assert_eq!(plan.entries[0].action, Action::MergeBoth);
        let done = apply(&plan, &backup, true).unwrap();
        assert_eq!(done.written, 2, "{:?}", done.failed);
        assert_eq!(slurp(&local, "src/a.java"), b"a\nL\nb\nc\nS\n");
        assert_eq!(slurp(&local, "src/a.java"), slurp(&server, "src/a.java"));
        put(&local, "src/a.java", b"a\nL2\nb\nc\nS\n", 100);
        let next = state_plan(&local, &server, &state);
        assert_eq!(next.entries[0].ancestor, AncInfo::Synced);
        assert!(next.entries[0].write_local.is_none());
        assert_eq!(next.entries[0].write_server.as_deref(), Some(b"a\nL2\nb\nc\nS\n".as_slice()));
        let back = restore(done.backup.as_ref().unwrap()).unwrap();
        assert_eq!(back.failed.len(), 1, "本地后续编辑不能被还原覆盖");
        assert_eq!(slurp(&local, "src/a.java"), b"a\nL2\nb\nc\nS\n");
        let stored = SyncState::load(&state, &local, &server).unwrap();
        assert!(!stored.files.contains_key("src/a.java"), "还原后不能继续使用合并后的基线");
    }

    #[test]
    fn reordering_lines_is_not_proof_of_a_common_base() {
        let (local, server, _) = tree("reordered_base");
        put(&local, "src/a.java", b"b\na\n", 100);
        put(&server, "src/a.java", b"a\nb\nS\n", 100);
        let server_stub = Stub::new().base_at("src/a.java", b"a\nb\n");
        let local_stub = Stub::new().state("src/a.java", WcState::Unversioned);
        let plan = plan(&local, &server, "", "", "", &server_stub, &local_stub, &MergeLimits::default()).unwrap();
        assert_eq!(plan.entries[0].action, Action::SkipUndecided);
        assert_eq!(plan.writing(), 0);
    }

    #[test]
    fn a_close_historical_version_is_not_guessed_as_the_ancestor() {
        let (local, server, _) = tree("similar_not_ancestor");
        put(&local, "src/a.java", b"a\nL\nb\nc\n", 100);
        put(&server, "src/a.java", b"a\nb\nc\nS\n", 100);
        let server_stub = Stub::new().base_at("src/a.java", b"a\nb\nc\nS\n")
            .rev("src/a.java", 1, b"a\nb\nc\n");
        let local_stub = Stub::new().state("src/a.java", WcState::Unversioned);
        let plan = plan(&local, &server, "", "", "", &server_stub, &local_stub, &MergeLimits::default()).unwrap();
        assert_eq!(plan.entries[0].action, Action::SkipUndecided);
    }

    #[test]
    fn same_size_and_same_timestamp_differences_are_verified_for_merging() {
        let (local, server, _, state) = synced_tree("same_metadata");
        put(&local, "src/a.java", b"a\nL\nc\n", 100);
        let plan = state_plan(&local, &server, &state);
        assert_eq!(plan.writing(), 1);
        assert_eq!(plan.entries[0].write_server.as_deref(), Some(b"a\nL\nc\n".as_slice()));
    }

    #[test]
    fn a_changed_sync_state_invalidates_an_older_preview() {
        let (local, server, backup, state) = synced_tree("state_changed");
        put(&local, "src/a.java", b"a\nL\nc\n", 100);
        let first = state_plan(&local, &server, &state);
        let _second = state_plan(&local, &server, &state);
        assert!(apply(&first, &backup, false).unwrap_err().contains("同步基线"));
        assert_eq!(slurp(&server, "src/a.java"), b"a\nb\nc\n");
    }

    #[test]
    fn broken_sync_snapshots_do_not_fall_back_to_guessing() {
        let (local, server, _, state_path) = synced_tree("broken_snapshot");
        let state = SyncState::load(&state_path, &local, &server).unwrap();
        let blob = state.files.get("src/a.java").unwrap();
        fs::remove_file(state_path.parent().unwrap().join("snapshots").join(blob)).unwrap();
        put(&local, "src/a.java", b"a\nL\nc\n", 100);
        let stub = Stub::new();
        let result = plan_with_state(&local, &server, "", "", "", &stub, &stub,
            &MergeLimits::default(), Some(&state_path));
        assert!(result.unwrap_err().contains("同步快照失败"));
        assert_eq!(slurp(&server, "src/a.java"), b"a\nb\nc\n");
    }

    #[test]
    fn backup_pruning_preserves_interrupted_sessions() {
        let (_, _, backup) = tree("prune_interrupted");
        let interrupted = backup.join(format!("{BACKUP_PREFIX}20000101-000000"));
        fs::create_dir_all(&interrupted).unwrap();
        append_journal(&interrupted, &JournalLine { kind: "session".into(), ..Default::default() }).unwrap();
        for i in 0..7 {
            let dir = backup.join(format!("{BACKUP_PREFIX}2026010{i}-000000"));
            fs::create_dir_all(&dir).unwrap();
            append_journal(&dir, &JournalLine { kind: "finished".into(), ..Default::default() }).unwrap();
        }
        prune(&backup, BACKUPS_KEPT);
        assert!(interrupted.is_dir());
        assert_eq!(backups(&backup).len(), BACKUPS_KEPT + 1);
    }

    #[test]
    #[cfg(windows)]
    fn real_svn_and_an_unversioned_development_directory_sync_safely() {
        use std::process::Command;
        for exe in ["svn", "svnadmin"] {
            if Command::new(exe).args(["--version", "--quiet"]).output().is_err() {
                eprintln!("{exe} 未安装，跳过本地 SVN 集成测试");
                return;
            }
        }
        let run = |exe: &str, args: &[&str]| {
            let output = Command::new(exe).args(args).output().unwrap();
            assert!(output.status.success(), "{exe} {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        };
        let (local, server, backup) = tree("real_svn_plain_dev");
        let repository = backup.parent().unwrap().join("repository");
        run("svnadmin", &["create", repository.to_str().unwrap()]);
        let url = format!("file:///{}", repository.to_string_lossy().replace('\\', "/"));
        run("svn", &["checkout", &url, server.to_str().unwrap()]);
        put(&server, "src/a.java", b"a\nb\nc\n", 100);
        run("svn", &["add", "--parents", server.join("src/a.java").to_str().unwrap()]);
        run("svn", &["commit", "-m", "initial", server.to_str().unwrap()]);
        put(&local, "src/a.java", b"a\nb\nc\n", 100);
        let state = backup.parent().unwrap().join("sync/state.json");
        let svn = crate::svn::Svn { exe: PathBuf::from("svn"), ..Default::default() };
        let preview = || {
            let anc = crate::svn::SvnAncestor::new(svn.clone(), &server);
            let local_anc = crate::svn::SvnAncestor::new_local(svn.clone(), &local);
            assert!(anc.error().is_none(), "{:?}", anc.error());
            assert!(local_anc.error().is_none(), "{:?}", local_anc.error());
            assert_eq!(local_anc.wc_state("src/a.java", local.join("src/a.java").exists()), WcState::Unversioned);
            plan_with_state(&local, &server, "", "", "", &anc, &local_anc,
                &MergeLimits::default(), Some(&state)).unwrap()
        };
        assert!(preview().entries.is_empty());
        run("svn", &["propset", "custom:owner", "preserved", server.join("src/a.java").to_str().unwrap()]);
        let properties = preview();
        assert!(properties.entries.iter().any(|e| e.action == Action::SkipMetadata));
        put(&local, "src/a.java", b"local edit\n", 100);
        let before_props_change = preview();
        run("svn", &["propset", "custom:owner", "new value", server.join("src/a.java").to_str().unwrap()]);
        assert!(apply(&before_props_change, &backup, false).unwrap_err().contains("属性已改变"));
        let with_properties = preview();
        let applied = apply(&with_properties, &backup, true).unwrap();
        assert_eq!(applied.written, 1, "{:?}", applied.failed);
        let property = Command::new("svn").args(["propget", "custom:owner", server.join("src/a.java").to_str().unwrap()]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&property.stdout).trim(), "new value");
        // Reset this independent property scenario before the deletion/history scenarios.
        run("svn", &["revert", server.join("src/a.java").to_str().unwrap()]);
        put(&local, "src/a.java", b"a\nb\nc\n", 100);
        preview();
        put(&local, "src/New.java", b"new\n", 100);
        let added = preview();
        assert_eq!(added.entries[0].action, Action::CopyLocalToServer);
        assert_eq!(apply(&added, &backup, true).unwrap().written, 1);
        put(&local, "src/a.java", b"a\nL\nb\nc\n", 100);
        put(&server, "src/a.java", b"a\nb\nc\nS\n", 100);
        let merged = apply(&preview(), &backup, true).unwrap();
        assert_eq!(merged.written, 2, "{:?}", merged.failed);
        let combined = slurp(&local, "src/a.java");
        assert_eq!(combined, b"a\nL\nb\nc\nS\n");
        run("svn", &["delete", "--force", server.join("src/a.java").to_str().unwrap()]);
        put(&local, "src/a.java", b"a\nnew local edit\nb\nc\nS\n", 100);
        assert_eq!(preview().entries[0].action, Action::SkipConflict);
        put(&local, "src/a.java", &combined, 100);
        let deleted = apply(&preview(), &backup, true).unwrap();
        assert_eq!(deleted.written, 1, "{:?}", deleted.failed);
        assert!(!local.join("src/a.java").exists());
        assert!(restore_with_client(deleted.backup.as_ref().unwrap(), Some(&svn)).unwrap().failed.is_empty());
        assert_eq!(slurp(&local, "src/a.java"), combined);
        run("svn", &["revert", server.join("src/a.java").to_str().unwrap()]);
        put(&local, "src/a.java", &slurp(&server, "src/a.java"), 100);
        assert!(preview().entries.is_empty());
        fs::remove_file(local.join("src/a.java")).unwrap();
        let plan = preview();
        assert_eq!(plan.entries[0].action, Action::DeleteServer);
        assert_eq!(apply(&plan, &backup, true).unwrap().written, 1);
        let (status, result) = svn.status(&server);
        assert!(result.ok);
        assert!(status.iter().any(|e| e.name == "a.java" && e.item == crate::svn::Item::Missing && e.item.uploadable()));

        // 同一路径切换分支后，旧清单不能落盘，旧基线也不能继续使用。
        run("svn", &["revert", server.join("src/a.java").to_str().unwrap()]);
        let sub_server = server.join("src");
        let sub_local = local.join("src");
        put(&sub_local, "a.java", &fs::read(sub_server.join("a.java")).unwrap(), 100);
        let sub_state = sync_state_path(backup.parent().unwrap(), &sub_local, &sub_server);
        let sub_preview = || {
            let server_anc = crate::svn::SvnAncestor::new(svn.clone(), &sub_server);
            let local_anc = crate::svn::SvnAncestor::new_local(svn.clone(), &sub_local);
            plan_with_state(&sub_local, &sub_server, "", "", "", &server_anc, &local_anc,
                &MergeLimits::default(), Some(&sub_state)).unwrap()
        };
        assert!(sub_preview().entries.is_empty());
        put(&sub_local, "a.java", b"local pending\n", 100);
        let before_switch = sub_preview();
        assert_eq!(before_switch.writing(), 1);
        run("svn", &["copy", &format!("{url}/src"), &format!("{url}/branch"), "-m", "branch"]);
        run("svn", &["switch", &format!("{url}/branch"), sub_server.to_str().unwrap()]);
        assert!(apply(&before_switch, &backup, false).unwrap_err().contains("仓库或分支"));
        let after_switch = sub_preview();
        assert!(after_switch.binding_reset);
        assert_eq!(after_switch.writing(), 0);

        // 字节没有变化、但 SVN 已计划删除时，也必须拒绝旧预览。
        put(&sub_local, "a.java", &fs::read(sub_server.join("a.java")).unwrap(), 100);
        sub_preview();
        put(&sub_local, "a.java", b"another pending edit\n", 100);
        let before_status_change = sub_preview();
        run("svn", &["delete", "--keep-local", sub_server.join("a.java").to_str().unwrap()]);
        assert!(apply(&before_status_change, &backup, false).unwrap_err().contains("SVN 状态已改变"));
    }

    #[test]
    fn an_interrupted_session_is_flagged_for_restore_first() {
        let (_local, _server, backup) = tree("interrupted");
        let dir = backup.join(format!("{BACKUP_PREFIX}20260101-120000"));
        fs::create_dir_all(&dir).unwrap();
        append_journal(&dir, &JournalLine { kind: "session".into(), ..Default::default() }).unwrap();
        append_journal(
            &dir,
            &JournalLine { kind: "begin".into(), rel: "src/a.java".into(), writes: vec![Record {
                side: "local".into(), rel: "src/a.java".into(), dest: backup.join("target.java").display().to_string(),
                ..Default::default()
            }], ..Default::default() },
        )
        .unwrap();
        let found = unfinished_backup(&backup).unwrap();
        assert!(found.unfinished);
        assert_eq!(found.files, 1);
        append_journal(&dir, &JournalLine { kind: "finished".into(), ..Default::default() }).unwrap();
        assert!(unfinished_backup(&backup).is_none());
    }

    #[test]
    fn an_interrupted_backup_is_unblocked_only_after_a_complete_restore() {
        let (local, server, backup) = tree("restore_interrupted_complete");
        put(&local, "src/New.java", b"ours\n", 100);
        let stub = Stub::new().state("src/New.java", WcState::Unversioned);
        let plan = merge_plan(&local, &server, "", &stub);
        let done = apply(&plan, &backup, true).unwrap();
        let dir = done.backup.unwrap();
        let mut raw = String::new();
        for line in read_journal(&dir).unwrap().into_iter().filter(|l| l.kind != "finished") {
            raw.push_str(&serde_json::to_string(&line).unwrap());
            raw.push('\n');
        }
        fs::write(dir.join(JOURNAL), raw).unwrap();
        assert!(unfinished_backup(&backup).is_some());
        put(&server, "src/New.java", b"edit\n", 100);
        assert_eq!(restore(&dir).unwrap().failed.len(), 1);
        assert!(unfinished_backup(&backup).is_some());
        put(&server, "src/New.java", b"ours\n", 100);
        assert!(restore(&dir).unwrap().failed.is_empty());
        assert!(unfinished_backup(&backup).is_none());
    }

    #[test]
    fn recovery_does_not_depend_on_a_readable_or_writable_sync_state() {
        for corrupt in [true, false] {
            let (local, server, backup, state) = synced_tree(if corrupt { "recovery_bad_state" } else { "recovery_readonly_state" });
            put(&local, "src/a.java", b"a\nL\nc\n", 100);
            let done = apply(&state_plan(&local, &server, &state), &backup, true).unwrap();
            let dir = done.backup.unwrap();
            if corrupt { fs::write(&state, b"broken json").unwrap(); }
            else {
                let mut permissions = fs::metadata(&state).unwrap().permissions();
                permissions.set_readonly(true);
                fs::set_permissions(&state, permissions).unwrap();
            }
            let result = restore(&dir).unwrap();
            assert!(result.failed.is_empty(), "{:?}", result.failed);
            assert_eq!(slurp(&server, "src/a.java"), b"a\nb\nc\n");
            assert_eq!(slurp(&local, "src/a.java"), b"a\nL\nc\n");
            assert!(!recovery_acknowledged(&dir));
            assert_eq!(clean_backups(&backup).0, 0, "状态未确认失效前必须保留恢复凭据");
            if !corrupt {
                let mut permissions = fs::metadata(&state).unwrap().permissions();
                permissions.set_readonly(false);
                fs::set_permissions(&state, permissions).unwrap();
                let mut recovered = SyncState::load(&state, &local, &server).unwrap();
                assert!(!recovered.files.contains_key("src/a.java"));
                recovered.save(&state).unwrap();
                assert!(recovery_acknowledged(&dir));
            }
        }
    }

    #[test]
    fn a_corrupt_or_incomplete_journal_cannot_report_a_successful_restore() {
        for malformed in [true, false] {
            let (local, server, backup, state) = synced_tree(if malformed { "bad_json_journal" } else { "incomplete_journal" });
            put(&local, "src/a.java", b"a\nL\nc\n", 100);
            let done = apply(&state_plan(&local, &server, &state), &backup, true).unwrap();
            let dir = done.backup.unwrap();
            if malformed {
                use std::io::Write;
                fs::OpenOptions::new().append(true).open(dir.join(JOURNAL)).unwrap().write_all(b"{broken\n").unwrap();
            } else {
                append_journal(&dir, &JournalLine { kind: "begin".into(), rel: "src/missing.java".into(), ..Default::default() }).unwrap();
            }
            assert!(restore(&dir).unwrap_err().contains("日志"));
            assert_eq!(slurp(&server, "src/a.java"), b"a\nL\nc\n");
            assert!(backups(&backup)[0].unfinished);
            assert_eq!(clean_backups(&backup).0, 0);
        }
    }

    #[test]
    fn a_second_side_failure_rolls_back_the_first_side_with_or_without_backup() {
        for backup_enabled in [true, false] {
            let (local, server, backup, state) = synced_tree(if backup_enabled { "rollback_with_backup" } else { "rollback_direct" });
            put(&local, "src/a.java", b"a\nL\nb\nc\n", 100);
            put(&server, "src/a.java", b"a\nb\nc\nS\n", 100);
            let plan = state_plan(&local, &server, &state);
            assert!(plan.entries[0].write_local.is_some() && plan.entries[0].write_server.is_some());
            FAIL_WRITE.with(|failure| *failure.borrow_mut() = Some(server.join("src/a.java")));
            let done = apply(&plan, &backup, backup_enabled);
            FAIL_WRITE.with(|failure| *failure.borrow_mut() = None);
            let done = done.unwrap();
            assert_eq!(done.written, 0, "{:?}", done.failed);
            assert_eq!(slurp(&local, "src/a.java"), b"a\nL\nb\nc\n");
            assert_eq!(slurp(&server, "src/a.java"), b"a\nb\nc\nS\n");
            assert!(done.failed.iter().any(|message| message.contains("模拟目标写入失败")));
            let state = SyncState::load(&state, &local, &server).unwrap();
            assert_eq!(state.files.len(), 1, "失败的合并不能推进同步基线");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_read_lease_prevents_an_in_place_editor_save() {
        let (local, _, _) = tree("editor_read_lease");
        let path = local.join("a.java");
        fs::write(&path, b"original").unwrap();
        let lease = read_lease(&path).unwrap();
        assert!(fs::write(&path, b"editor edit").is_err());
        drop(lease);
        fs::write(&path, b"editor edit").unwrap();
    }

    #[test]
    fn a_fresh_install_never_replaces_a_file_created_by_someone_else() {
        let (local, _, _) = tree("fresh_no_clobber");
        let path = local.join("a.java");
        fs::write(&path, b"someone else's file").unwrap();
        assert!(replace_mode(&path, b"our output", false).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"someone else's file");
    }

    /// 「清理备份」删的只有已经完成的那几次，写到一半断掉的那次留着当还原凭据；
    /// 报出去的条数与体积都只算真的从盘上消失的那些
    #[test]
    fn cleaning_backups_removes_finished_sessions_only() {
        let (_local, _server, backup) = tree("clean_backups");
        let done_dir = backup.join(format!("{BACKUP_PREFIX}20260102-120000"));
        put(&done_dir, "local/a.java", b"old".as_slice(), 100);
        append_journal(&done_dir, &JournalLine { kind: "session".into(), ..Default::default() })
            .unwrap();
        append_journal(&done_dir, &JournalLine { kind: "finished".into(), ..Default::default() })
            .unwrap();
        let half_dir = backup.join(format!("{BACKUP_PREFIX}20260101-120000"));
        put(&half_dir, "local/a.java", b"older".as_slice(), 100);
        append_journal(&half_dir, &JournalLine { kind: "session".into(), ..Default::default() })
            .unwrap();
        let (removed, bytes) = clean_backups(&backup);
        assert_eq!(removed, 1, "只该删掉已完成的那次");
        assert_eq!(bytes, 3, "释放的字节只算删掉那个目录里的内容，journal 不计");
        assert!(!done_dir.exists(), "已完成的那次整目录消失");
        assert!(half_dir.exists(), "未完成的那次是唯一的还原凭据，得留着");
    }

    /// 两边各自还有一处改动、又都加了同一行：那一行只应用一次
    #[test]
    fn an_identical_edit_on_both_sides_is_applied_once() {
        let (local, server, _backup) = tree("same_edit");
        let anc = text(&["x", "a", "b", "y"]);
        put(&local, "src/a.java", text(&["x", "shared", "a", "L", "b", "y"]).as_bytes(), 400);
        put(&server, "src/a.java", text(&["x", "shared", "a", "b", "S", "y"]).as_bytes(), 300);
        let stub = Stub::new().base_at("src/a.java", anc.as_bytes());
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::MergeBoth);
        let want = ["x", "shared", "a", "L", "b", "S", "y"];
        assert_eq!(keys(entry.write_local.as_ref().unwrap()), want);
        assert_eq!(keys(entry.write_server.as_ref().unwrap()), want);
        assert_eq!(
            entry.regions.iter().filter(|r| r.kind == RegionKind::BothSame).count(),
            1,
            "相同的那处要认出来，regions = {:?}",
            entry.regions
        );
    }

    /// 同一行两边改得不一样：整文件一个字节都不动，只进通知清单
    #[test]
    fn a_conflict_writes_nothing_and_is_listed_for_manual_handling() {
        let (local, server, _backup) = tree("conflict");
        let anc = text(&["a", "b", "c"]);
        put(&local, "src/a.java", text(&["a", "L", "c"]).as_bytes(), 400);
        put(&server, "src/a.java", text(&["a", "S", "c"]).as_bytes(), 300);
        let stub = Stub::new().base_at("src/a.java", anc.as_bytes());
        let plan = merge_plan(&local, &server, "", &stub);
        let entry = &plan.entries[0];
        assert_eq!(entry.action, Action::SkipConflict);
        assert!(!entry.action.is_write());
        assert!(entry.write_local.is_none() && entry.write_server.is_none());
        assert_eq!(entry.regions_text(), "本地 2–2 / 服务器 2–2");
        assert_eq!(plan.writing(), 0);
        assert_eq!(plan.conflicts(), 1);
    }

    /// 已经一致（含只差 BOM / 行尾）的对不占清单：它们没有可合的东西，但要比过的数
    #[test]
    fn pairs_that_already_agree_are_not_listed() {
        let (local, server, _backup) = tree("agreed");
        put(&server, "src/a.java", windows_text(&["a", "b"], true).as_bytes(), 300);
        put(&local, "src/a.java", text(&["a", "b"]).as_bytes(), 400);
        let stub = Stub::new();
        let plan = merge_plan(&local, &server, "", &stub);
        assert!(plan.entries.is_empty());
        assert_eq!(plan.checked, 1, "比过要报得出数");
    }
}
