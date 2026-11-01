//! workspace 的多个根（ADR-45）：把配置里的主根和 `workspace.extra_roots` 展开成一组根，校验它们之间、以及和 anycon 数据目录
//! 的关系，并判定每个根现在能不能用。
//!
//! 只做判断，不创建目录、不碰会话。daemon 启动（`workspace.rs`）和 `anycon doctor`（`doctor.rs`）用同一个入口，所以 doctor 说的
//! 和 daemon 实际做的一致。
//!
//! 两类问题，处理不同：
//! - **配置错误**（[`RootsError`]）：额外根和别的根相同或嵌套、在数据目录里或包含数据目录、是文件系统根。这些不会因为一块盘
//!   插上 / 拔掉而变，daemon 拒绝启动。「相同 / 嵌套 / 包含」除了路径前缀，还按目录身份 `(dev, ino)` 沿祖先链判断（和关联目录的
//!   绝对拒绝同一个做法，ADR-33），同一个目录的另一种写法（符号链接、大小写变体）绕不过去；不存在的根只有路径前缀可比。
//! - **根暂时用不了**：额外根不存在或不是目录（外接盘没插）、`~` 展开不了。跳过它并 warn，不影响启动，doctor 里对应的一项失败。
//!
//! 主根不在这里判可用性（`Workspace::new` 会创建它），也不套数据目录 / 文件系统根的检查：沿用 v0.1 的行为，不让升级之后
//! 原来能启动的配置起不来；它只参与「和额外根不能重叠」。

use std::fmt;
use std::path::{Path, PathBuf};

use crate::linkrules::{lexical_clean, Identities};
use crate::util::expand_tilde;

/// 配置里的一个根，展开、判定之后的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRoot {
    /// 配置里的原样（主根是 `workspace.root` 的值）。
    pub configured: String,
    /// 可用的根是 canonical 路径；不可用的根是 `~` 展开并做过词法规范化的路径（展开不了时是原样）。
    pub path: PathBuf,
    /// 主根（`workspace.root`）。
    pub primary: bool,
    /// 不能用的原因；`None` = 可用。
    pub unavailable: Option<String>,
}

/// 两个根之间的关系（第一个相对第二个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Same,
    Inside,
    Contains,
}

impl fmt::Display for Relation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Relation::Same => "is the same directory as",
            Relation::Inside => "is inside",
            Relation::Contains => "contains",
        })
    }
}

/// 配置错误：daemon 拒绝启动。消息直接给运维看，说明哪一项、为什么。
#[derive(Debug, thiserror::Error)]
pub enum RootsError {
    #[error(
        "workspace.extra_roots: {entry} {relation} {other}; workspace roots must be different directories and must not be nested"
    )]
    Overlap { entry: String, relation: Relation, other: String },
    #[error(
        "workspace.extra_roots: {entry} {} the anycon data directory ({data}); a workspace root must not hold the data directory or live inside it",
        if *.inside { "is, or is inside," } else { "contains" }
    )]
    DataDir { entry: String, inside: bool, data: String },
    #[error("workspace.extra_roots: {entry} is a filesystem root; list a directory on that filesystem instead")]
    FilesystemRoot { entry: String },
}

/// 按配置展开所有根：主根在最前，额外根按配置顺序。`data_dir` 是 anycon 数据目录（`ANYCON_HOME`）。
pub fn plan(cfg: &crate::config::Config, data_dir: &Path) -> Result<Vec<PlannedRoot>, RootsError> {
    plan_with(&cfg.workspace.root, &cfg.workspace_root(), &cfg.workspace.extra_roots, data_dir)
}

/// [`plan`]，主根由调用方给（`configured` 是它在配置里的写法，`primary` 是展开后的路径；调用方可能已经创建并 canonicalize 过）。
pub fn plan_with(
    configured: &str,
    primary: &Path,
    extra: &[String],
    data_dir: &Path,
) -> Result<Vec<PlannedRoot>, RootsError> {
    let primary_path = crate::sys::canonicalize(primary)
        .or_else(|_| std::path::absolute(primary).map(|p| lexical_clean(&p)))
        .unwrap_or_else(|_| primary.to_path_buf());
    let mut roots =
        vec![PlannedRoot { configured: configured.to_string(), path: primary_path, primary: true, unavailable: None }];
    for entry in extra {
        roots.push(plan_extra(entry));
    }
    check(&roots, data_dir)?;
    Ok(roots)
}

/// 一个额外根：展开 `~`，canonicalize，必须是目录。
fn plan_extra(entry: &str) -> PlannedRoot {
    let unavailable = |path: PathBuf, why: String| PlannedRoot {
        configured: entry.to_string(),
        path,
        primary: false,
        unavailable: Some(why),
    };
    let expanded = expand_tilde(entry);
    if !expanded.is_absolute() {
        return unavailable(
            expanded,
            "it cannot be expanded (the environment has no usable home directory for `~`)".into(),
        );
    }
    let lexical = lexical_clean(&expanded);
    match crate::sys::canonicalize(&lexical) {
        Ok(canon) if canon.is_dir() => {
            PlannedRoot { configured: entry.to_string(), path: canon, primary: false, unavailable: None }
        }
        Ok(_) => unavailable(lexical, "it is not a directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            unavailable(lexical, "the directory does not exist (is the disk mounted?)".into())
        }
        Err(e) => unavailable(lexical, format!("the directory cannot be read: {e}")),
    }
}

fn describe(r: &PlannedRoot) -> String {
    let name = if r.primary { "workspace.root" } else { "an entry" };
    let shown = r.path.display().to_string();
    if r.configured == shown {
        format!("{} ({name})", r.configured)
    } else {
        format!("{} ({name}, {shown})", r.configured)
    }
}

/// 额外根的静态检查（见模块文档）。
fn check(roots: &[PlannedRoot], data_dir: &Path) -> Result<(), RootsError> {
    let ids: Vec<Identities> = roots
        .iter()
        .map(|r| if r.unavailable.is_none() { Identities::of(&r.path) } else { Identities::default() })
        .collect();
    let data_lexical = lexical_clean(data_dir);
    let data = crate::sys::canonicalize(data_dir).unwrap_or_else(|_| data_lexical.clone());
    let data_ids = Identities::of(&data);

    for (i, r) in roots.iter().enumerate().skip(1) {
        if r.path.parent().is_none() {
            return Err(RootsError::FilesystemRoot { entry: describe(r) });
        }
        // 数据目录：在里面（含等于）或包含它。路径前缀 + 目录身份
        let inside = r.path.starts_with(&data)
            || r.path.starts_with(&data_lexical)
            || data_ids.me.is_some_and(|m| ids[i].chain.contains(&m));
        let contains = data.starts_with(&r.path)
            || data_lexical.starts_with(&r.path)
            || ids[i].me.is_some_and(|m| data_ids.chain.contains(&m));
        if inside || contains {
            return Err(RootsError::DataDir { entry: describe(r), inside, data: data.display().to_string() });
        }
    }
    // 两两之间（主根也算）：后面的那一项是出问题的那个
    for j in 1..roots.len() {
        for i in 0..j {
            if let Some(relation) = relation(&roots[j].path, &ids[j], &roots[i].path, &ids[i]) {
                return Err(RootsError::Overlap { entry: describe(&roots[j]), relation, other: describe(&roots[i]) });
            }
        }
    }
    Ok(())
}

/// `a` 相对 `b` 的关系：路径前缀，或目录身份（`b` 在 `a` 的祖先链里 = `a` 在 `b` 里面，反过来同理）。
fn relation(a: &Path, a_ids: &Identities, b: &Path, b_ids: &Identities) -> Option<Relation> {
    let same_dir = a_ids.me.is_some() && a_ids.me == b_ids.me;
    if a == b || same_dir {
        return Some(Relation::Same);
    }
    if a.starts_with(b) || b_ids.me.is_some_and(|m| a_ids.chain.contains(&m)) {
        return Some(Relation::Inside);
    }
    if b.starts_with(a) || a_ids.me.is_some_and(|m| b_ids.chain.contains(&m)) {
        return Some(Relation::Contains);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _d: tempfile::TempDir,
        base: PathBuf,
        home: PathBuf,
    }

    fn env() -> Env {
        let d = tempfile::tempdir().unwrap();
        let base = crate::sys::canonicalize(d.path()).unwrap();
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Env { _d: d, base, home }
    }

    impl Env {
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.base.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
        fn plan(&self, primary: &Path, extra: &[&Path]) -> Result<Vec<PlannedRoot>, RootsError> {
            let extra: Vec<String> = extra.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            plan_with(&primary.to_string_lossy(), primary, &extra, &self.home)
        }
    }

    #[test]
    fn extra_roots_are_listed_after_the_primary_in_configuration_order() {
        let e = env();
        let (primary, a, b) = (e.dir("ws"), e.dir("a"), e.dir("b"));
        let roots = e.plan(&primary, &[&b, &a]).unwrap();
        assert_eq!(roots.iter().map(|r| r.path.clone()).collect::<Vec<_>>(), vec![primary, b, a]);
        assert_eq!(roots.iter().map(|r| r.primary).collect::<Vec<_>>(), vec![true, false, false]);
        assert!(roots.iter().all(|r| r.unavailable.is_none()));
    }

    #[test]
    fn a_missing_or_non_directory_extra_root_is_skipped_not_an_error() {
        let e = env();
        let primary = e.dir("ws");
        let missing = e.base.join("unplugged");
        let file = e.base.join("a-file");
        std::fs::write(&file, "x").unwrap();
        let usable = e.dir("usable");
        let roots = e.plan(&primary, &[&missing, &file, &usable]).unwrap();
        assert!(roots[1].unavailable.as_deref().unwrap().contains("does not exist"), "{:?}", roots[1]);
        assert_eq!(roots[1].path, missing, "an unavailable root keeps its expanded path");
        assert!(roots[2].unavailable.as_deref().unwrap().contains("not a directory"), "{:?}", roots[2]);
        assert!(roots[3].unavailable.is_none());
    }

    #[test]
    fn the_same_directory_twice_is_a_configuration_error() {
        let e = env();
        let (primary, a) = (e.dir("ws"), e.dir("a"));
        let err = e.plan(&primary, &[&a, &a]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Same, .. }), "{err}");
        // 和主根相同
        let err = e.plan(&primary, &[&primary]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Same, .. }), "{err}");
        assert!(err.to_string().contains("workspace.root"), "the message names the primary root: {err}");
    }

    #[test]
    fn nested_roots_are_rejected_in_both_directions() {
        let e = env();
        let (primary, a) = (e.dir("ws"), e.dir("a"));
        let inner = e.dir("a/inner");
        let err = e.plan(&primary, &[&a, &inner]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Inside, .. }), "{err}");
        let err = e.plan(&primary, &[&inner, &a]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Contains, .. }), "{err}");
        // 在主根里面 / 包含主根
        let under_primary = e.dir("ws/ext");
        assert!(matches!(
            e.plan(&primary, &[&under_primary]).unwrap_err(),
            RootsError::Overlap { relation: Relation::Inside, .. }
        ));
        assert!(matches!(
            e.plan(&under_primary, &[&primary]).unwrap_err(),
            RootsError::Overlap { relation: Relation::Contains, .. }
        ));
        // 兄弟目录、名字有公共前缀的目录不算嵌套
        let (s1, s2) = (e.dir("proj"), e.dir("proj-2"));
        assert!(e.plan(&primary, &[&s1, &s2]).is_ok());
    }

    #[test]
    fn nesting_is_judged_by_directory_identity_not_by_the_spelling() {
        let e = env();
        let (primary, a) = (e.dir("ws"), e.dir("a"));
        e.dir("a/inner");
        // 带 `..` 的写法
        let dotdot = e.base.join("a/inner/..");
        let err = e.plan(&primary, &[&a, &dotdot]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Same, .. }), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_does_not_hide_that_two_roots_are_nested_or_the_same() {
        let e = env();
        let (primary, a) = (e.dir("ws"), e.dir("a"));
        let inner = e.dir("a/inner");
        // 符号链接指向 `a/inner`：canonicalize 之后就是它，`a` 包含它
        let link = e.base.join("link-to-inner");
        std::os::unix::fs::symlink(&inner, &link).unwrap();
        let err = e.plan(&primary, &[&a, &link]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Inside, .. }), "{err}");
        // 同一个目录的两种写法
        let link2 = e.base.join("link-to-a");
        std::os::unix::fs::symlink(&a, &link2).unwrap();
        let err = e.plan(&primary, &[&a, &link2]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Same, .. }), "{err}");
    }

    #[test]
    fn overlap_with_a_root_that_is_not_there_is_judged_by_path() {
        let e = env();
        let primary = e.dir("ws");
        let (a, a_inner) = (e.base.join("gone"), e.base.join("gone/inner"));
        let err = e.plan(&primary, &[&a, &a_inner]).unwrap_err();
        assert!(matches!(err, RootsError::Overlap { relation: Relation::Inside, .. }), "{err}");
    }

    #[test]
    fn a_root_that_holds_or_lives_in_the_data_directory_is_rejected() {
        let e = env();
        let primary = e.dir("ws");
        // 包含数据目录
        let err = e.plan(&primary, &[&e.base]).unwrap_err();
        assert!(matches!(err, RootsError::DataDir { inside: false, .. }), "{err}");
        // 就是数据目录、在数据目录里面
        let err = e.plan(&primary, &[&e.home]).unwrap_err();
        assert!(matches!(err, RootsError::DataDir { inside: true, .. }), "{err}");
        let under = e.dir("home/projects");
        let err = e.plan(&primary, &[&under]).unwrap_err();
        assert!(matches!(err, RootsError::DataDir { inside: true, .. }), "{err}");
        assert!(err.to_string().contains("data directory"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_filesystem_root_is_rejected() {
        let e = env();
        let primary = e.dir("ws");
        let err = e.plan(&primary, &[Path::new("/")]).unwrap_err();
        assert!(matches!(err, RootsError::FilesystemRoot { .. }), "{err}");
    }

    #[test]
    fn the_primary_root_is_not_subject_to_the_data_directory_check() {
        // 沿用 v0.1：主根的位置不受限制（只和额外根比较），升级之后原来能启动的配置照样能启动
        let e = env();
        let a = e.dir("a");
        let roots = plan_with(&e.base.to_string_lossy(), &e.base, &[a.to_string_lossy().into_owned()], &e.home);
        assert!(matches!(roots.unwrap_err(), RootsError::Overlap { .. }), "the extra root is inside the primary");
        let ok = plan_with(&e.base.to_string_lossy(), &e.base, &[], &e.home).unwrap();
        assert_eq!(ok.len(), 1);
    }

    #[test]
    fn plan_reads_the_roots_from_the_configuration() {
        let e = env();
        let (primary, a) = (e.dir("ws"), e.dir("a"));
        let mut cfg = crate::config::Config::default();
        cfg.workspace.root = primary.to_string_lossy().into_owned();
        cfg.workspace.extra_roots =
            vec![a.to_string_lossy().into_owned(), e.base.join("later").to_string_lossy().into_owned()];
        let roots = plan(&cfg, &e.home).unwrap();
        assert_eq!(roots.len(), 3);
        assert!(roots[0].primary && roots[1].unavailable.is_none() && roots[2].unavailable.is_some());
    }
}
