//! 行级的 unified diff。ACP 的 `diff` 内容块只给整个文件修改前 / 修改后的文本（`oldText` / `newText`），
//! `tool_result.detail.unified_diff` 要的是 unified 格式，所以这里自己算。
//!
//! Myers 的 O(ND) 算法，带上限：任何一边超过 [`MAX_LINES`] 行，或者编辑距离超过 [`MAX_EDIT_DISTANCE`]，就退回「整个文件
//! 换掉」（全部旧行 `-`、全部新行 `+`，一个 hunk），免得一个巨大的文件拖住读事件的线程。两边合计超过 [`MAX_BYTES`] 的
//! 文本不算 diff（返回 `None`）。上下文 3 行，和 `git diff` 默认一样。

const CONTEXT: usize = 3;
pub const MAX_LINES: usize = 20_000;
pub const MAX_EDIT_DISTANCE: usize = 2_000;
pub const MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// 逐行比较：返回每一步是保留 / 删除（旧文件的一行）/ 插入（新文件的一行）。超过上限返回 `None`。
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    if max == 0 {
        return Some(Vec::new());
    }
    let cap = max.min(MAX_EDIT_DISTANCE);
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    // trace[d] 是第 d 轮开始前 v 里 [-(d+1), d+1] 这一段（回溯只需要它）
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=cap as isize {
        trace.push(v[(off - d - 1) as usize..=(off + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let at = |k: isize| v[(off + k) as usize];
            let mut x = if k == -d || (k != d && at(k - 1) < at(k + 1)) { at(k + 1) } else { at(k - 1) + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(off + k) as usize] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let (mut x, mut y) = (n, m);
    let mut ops = Vec::new();
    for d in (0..trace.len() as isize).rev() {
        let t = &trace[d as usize];
        let at = |k: isize| t[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == prev_x { Op::Insert } else { Op::Delete });
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

fn lines(s: &str) -> Vec<&str> {
    s.lines().collect()
}

fn range(start: usize, len: usize) -> String {
    match len {
        1 => format!("{start}"),
        // 空范围的起始行是它前面那一行
        0 => format!("{},0", start.saturating_sub(1)),
        _ => format!("{start},{len}"),
    }
}

/// `old = None` 表示新建的文件。`old_name` / `new_name` 是文件头里的名字（`a/…` / `b/…` 或 `/dev/null`）。
/// 内容没有变化返回空串；文本太大返回 `None`。
pub fn unified_diff(old_name: &str, new_name: &str, old: Option<&str>, new: &str) -> Option<String> {
    let old_text = old.unwrap_or("");
    if old_text.len() + new.len() > MAX_BYTES {
        return None;
    }
    if old.is_some() && old_text == new {
        return Some(String::new());
    }
    let (a, b) = (lines(old_text), lines(new));
    let ops = if a.len() > MAX_LINES || b.len() > MAX_LINES { None } else { myers(&a, &b) }.unwrap_or_else(|| {
        // 整个文件换掉
        std::iter::repeat_n(Op::Delete, a.len()).chain(std::iter::repeat_n(Op::Insert, b.len())).collect()
    });

    let mut out = format!("--- {old_name}\n+++ {new_name}\n");
    // 每一步对应旧 / 新文件里的行号（从 0 起），找出变化的下标再按上下文合并成 hunk
    let mut pos = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0usize, 0usize);
    for op in &ops {
        pos.push((i, j));
        match op {
            Op::Equal => {
                i += 1;
                j += 1;
            }
            Op::Delete => i += 1,
            Op::Insert => j += 1,
        }
    }
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| **o != Op::Equal).map(|(k, _)| k).collect();
    if changed.is_empty() {
        // 只差末尾换行之类逐行比较看不出的差别
        return Some(String::new());
    }
    let mut idx = 0;
    while idx < changed.len() {
        let mut first = changed[idx];
        let mut last = changed[idx];
        idx += 1;
        // 两处变化之间的相同行不超过 2 * CONTEXT 就并成一个 hunk
        while idx < changed.len() && changed[idx] - last <= 2 * CONTEXT + 1 {
            last = changed[idx];
            idx += 1;
        }
        first = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT + 1).min(ops.len());
        let (old_start, new_start) = pos[first];
        let old_len = ops[first..end].iter().filter(|o| **o != Op::Insert).count();
        let new_len = ops[first..end].iter().filter(|o| **o != Op::Delete).count();
        out.push_str(&format!("@@ -{} +{} @@\n", range(old_start + 1, old_len), range(new_start + 1, new_len)));
        for k in first..end {
            let (i, j) = pos[k];
            match ops[k] {
                Op::Equal => out.push_str(&format!(" {}\n", a[i])),
                Op::Delete => out.push_str(&format!("-{}\n", a[i])),
                Op::Insert => out.push_str(&format!("+{}\n", b[j])),
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(old: Option<&str>, new: &str) -> String {
        let (o, n) = if old.is_some() { ("a/f.txt", "b/f.txt") } else { ("/dev/null", "b/f.txt") };
        unified_diff(o, n, old, new).unwrap()
    }

    #[test]
    fn a_new_file_is_all_additions() {
        assert_eq!(diff(None, "hi\nthere\n"), "--- /dev/null\n+++ b/f.txt\n@@ -0,0 +1,2 @@\n+hi\n+there\n");
    }

    #[test]
    fn a_replaced_line_has_context_and_a_hunk_header() {
        assert_eq!(diff(Some("x\n"), "y\n"), "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-x\n+y\n");
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = "1\n2\n3\n4\nfive\n6\n7\n8\n";
        assert_eq!(
            diff(Some(old), new),
            "--- a/f.txt\n+++ b/f.txt\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn distant_changes_make_separate_hunks_and_near_ones_merge() {
        let old: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let far = old.replace("\n3\n", "\nthree\n").replace("\n28\n", "\ntwenty-eight\n");
        let d = diff(Some(&old), &far);
        assert_eq!(d.matches("@@ ").count(), 2, "{d}");
        let near = old.replace("\n3\n", "\nthree\n").replace("\n8\n", "\neight\n");
        let d = diff(Some(&old), &near);
        assert_eq!(d.matches("@@ ").count(), 1, "{d}");
        assert!(d.contains("-3\n+three\n") && d.contains("-8\n+eight\n"));
    }

    #[test]
    fn insertions_deletions_and_unchanged_text() {
        assert_eq!(diff(Some("a\nb\nc\n"), "a\nc\n"), "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,2 @@\n a\n-b\n c\n");
        assert_eq!(diff(Some("a\nc\n"), "a\nb\nc\n"), "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,3 @@\n a\n+b\n c\n");
        assert_eq!(diff(Some("same\n"), "same\n"), "");
        // 清空文件
        assert_eq!(diff(Some("a\nb\n"), ""), "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +0,0 @@\n-a\n-b\n");
        // 空文件变成有内容
        assert_eq!(diff(Some(""), "a\n"), "--- a/f.txt\n+++ b/f.txt\n@@ -0,0 +1 @@\n+a\n");
    }

    /// 把 diff 打到旧文本上：hunk 之外的行原样保留，hunk 里的上下文和删除行要和旧文本对得上。
    fn apply(old: &[String], d: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut oi = 0usize;
        for line in d.lines().skip(2) {
            if let Some(h) = line.strip_prefix("@@ -") {
                let spec = h.split(' ').next().unwrap();
                let mut it = spec.split(',');
                let s: usize = it.next().unwrap().parse().unwrap();
                let l: usize = it.next().map(|x| x.parse().unwrap()).unwrap_or(1);
                let start = if l == 0 { s } else { s - 1 };
                while oi < start {
                    out.push(old[oi].clone());
                    oi += 1;
                }
                continue;
            }
            let (tag, body) = line.split_at(1);
            match tag {
                " " => {
                    assert_eq!(old[oi], body, "context line mismatch in\n{d}");
                    out.push(body.to_string());
                    oi += 1;
                }
                "-" => {
                    assert_eq!(old[oi], body, "deleted line mismatch in\n{d}");
                    oi += 1;
                }
                "+" => out.push(body.to_string()),
                other => panic!("bad tag {other}"),
            }
        }
        out.extend(old[oi..].iter().cloned());
        out
    }

    #[test]
    fn applying_the_diff_to_the_old_text_gives_the_new_text() {
        // 一批确定的伪随机文本（行取自很小的字母表，重复行多，最容易把算法弄错）
        let mut seed = 12345u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..300 {
            let old: Vec<String> = (0..next() % 30).map(|_| format!("l{}", next() % 5)).collect();
            let new: Vec<String> = (0..next() % 30).map(|_| format!("l{}", next() % 5)).collect();
            let (o, n) = (old.join("\n"), new.join("\n"));
            let d = unified_diff("a", "b", Some(&o), &n).unwrap();
            if old == new {
                assert_eq!(d, "");
                continue;
            }
            assert_eq!(apply(&old, &d), new, "diff\n{d}\nold {old:?}\nnew {new:?}");
        }
    }

    #[test]
    fn huge_inputs_degrade_instead_of_blowing_up() {
        // 行数过多：整个文件换掉
        let old: String = (0..MAX_LINES + 5).map(|i| format!("a{i}\n")).collect();
        let new: String = (0..MAX_LINES + 5).map(|i| format!("b{i}\n")).collect();
        let d = unified_diff("a", "b", Some(&old), &new).unwrap();
        assert!(d.starts_with("--- a\n+++ b\n@@ -1,20005 +1,20005 @@\n-a0\n"), "{}", &d[..80]);
        // 太大：不算 diff
        let big = "x\n".repeat(MAX_BYTES);
        assert_eq!(unified_diff("a", "b", Some(&big), "y\n"), None);
        // 编辑距离超过上限：整个文件换掉（只要不 panic、结果合理）
        let old: String = (0..3000).map(|i| format!("a{i}\n")).collect();
        let new: String = (0..3000).map(|i| format!("b{i}\n")).collect();
        let d = unified_diff("a", "b", Some(&old), &new).unwrap();
        assert!(d.contains("-a0\n") && d.contains("+b2999\n"));
    }
}
