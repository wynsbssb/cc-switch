//! Codex 会话/状态数据统一存放。
//!
//! 目标：把 Codex 的会话数据（sessions / archived_sessions）和线程状态库
//! （state_5.sqlite）物理统一到 `~/.cc-switch/codex/` 下一个固定位置，所有
//! 供应商/账号/路由只改 `~/.codex/config.toml` 与 `auth.json`，会话数据本身
//! 永远不动 —— 切换路由不再需要逐次复制/备份数百 MB 会话。
//!
//! 实现方式：
//! - `~/.codex/sessions` -> junction/symlink -> `~/.cc-switch/codex/sessions`
//! - `~/.codex/archived_sessions` -> junction/symlink -> `~/.cc-switch/codex/archived_sessions`
//! - 状态库由 live config 注入 `sqlite_home` 指向 `~/.cc-switch/codex/state`
//!   （见 `codex_config::inject_codex_unified_state_home`）。
//! Codex/桌面端始终读写 `~/.codex/sessions`，经 junction 透明落到统一目录，
//! cc-switch 的会话管理/用量/快照也走同一路径，天然“统一调用”。
//!
//! 所有操作 best-effort 且幂等：启用时先迁移已有数据、再建链接；启动时若
//! 标记存在但链接丢失会自动重建；关闭时把数据移回 `~/.codex` 并撤链接。

use std::fs;
use std::path::{Path, PathBuf};

use crate::codex_state_db::CODEX_STATE_DB_FILENAME;
use crate::config::{atomic_write, copy_file, get_app_config_dir};
use crate::error::AppError;

/// 统一存储已激活的标记文件（放在统一目录下）。
const CODEX_UNIFIED_STORAGE_MARKER: &str = ".unified-active";

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexUnifiedStorageOutcome {
    pub enabled: bool,
    pub active: bool,
    pub codex_dir: String,
    pub sessions_dir: String,
    pub archived_dir: String,
    pub state_dir: String,
    pub migrated_sessions: usize,
    pub migrated_archived: usize,
    pub migrated_state_dbs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

/// 统一会话/状态数据根目录：`~/.cc-switch/codex`。
pub fn unified_home() -> PathBuf {
    get_app_config_dir().join("codex")
}

pub fn sessions_dir() -> PathBuf {
    unified_home().join("sessions")
}

pub fn archived_dir() -> PathBuf {
    unified_home().join("archived_sessions")
}

pub fn state_dir() -> PathBuf {
    unified_home().join("state")
}

pub fn state_db_path() -> PathBuf {
    state_dir().join(CODEX_STATE_DB_FILENAME)
}

pub fn is_enabled() -> bool {
    crate::settings::unify_codex_session_storage()
}

/// 是否真正处于统一存储状态（开关打开且两个目录链接都在）。
pub fn is_active() -> bool {
    if !is_enabled() {
        return false;
    }
    let codex_dir = crate::codex_config::get_codex_config_dir();
    is_dir_link(&codex_dir.join("sessions")) && is_dir_link(&codex_dir.join("archived_sessions"))
}

pub fn status() -> CodexUnifiedStorageOutcome {
    let codex_dir = crate::codex_config::get_codex_config_dir();
    CodexUnifiedStorageOutcome {
        enabled: is_enabled(),
        active: is_active(),
        codex_dir: codex_dir.display().to_string(),
        sessions_dir: sessions_dir().display().to_string(),
        archived_dir: archived_dir().display().to_string(),
        state_dir: state_dir().display().to_string(),
        ..Default::default()
    }
}

/// 路径归一化键（仅用于本地一致性比较）：统一分隔符、Windows 小写、去卷前缀。
fn normalized_path_key(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        if let Some(stripped) = s.strip_prefix("//?/") {
            s = stripped.to_string();
        }
        s = s.to_lowercase();
    }
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// 统一存储必须与 Codex 配置目录同根，且绝不能把真实 `~/.codex` 会话迁进
/// 系统临时目录或另一个 home。历史事故：HOME/CC_SWITCH_TEST_HOME 残留时，
/// 应用配置目录指向临时目录而 Codex 目录仍是真实目录，enable() 曾把真实会话
/// 搬进临时目录后链接指向丢失目标，导致“对话数据全没了”。这里在迁移前直接
/// 拒绝，避免再次发生。
fn ensure_same_home_root(codex_dir: &Path) -> Result<(), AppError> {
    let app_root = get_app_config_dir().parent().map(normalized_path_key);
    let codex_root = codex_dir.parent().map(normalized_path_key);
    if app_root.is_some() && codex_root.is_some() && app_root == codex_root {
        // 同根时再兜一层：Codex 目录不在临时目录、统一目录却在临时目录，
        // 说明残留测试环境（CC_SWITCH_TEST_HOME/HOME 指向临时路径）。
        let temp_key = normalized_path_key(&std::env::temp_dir());
        let codex_under_temp = codex_root
            .as_deref()
            .map(|root| root.starts_with(&temp_key))
            .unwrap_or(false);
        let unified_under_temp = normalized_path_key(&unified_home()).starts_with(&temp_key);
        if !codex_under_temp && unified_under_temp {
            return Err(AppError::Message(format!(
                "统一存储目录 {} 位于系统临时目录下（Codex 目录 {} 不在），疑似残留 CC_SWITCH_TEST_HOME/HOME 测试环境，拒绝启用以免迁移真实会话",
                unified_home().display(),
                codex_dir.display()
            )));
        }
        return Ok(());
    }
    Err(AppError::Message(format!(
        "Codex 配置目录（{}）与应用配置目录（{}）不同根，无法安全启用统一存储；请检查 HOME / CC_SWITCH_TEST_HOME / app_config_dir 覆盖是否残留",
        codex_dir.display(),
        get_app_config_dir().display()
    )))
}

/// 启用统一存储：迁移已有数据到统一目录，然后在 `~/.codex` 下建目录链接。
/// 幂等：已迁移/已链接时直接跳过对应步骤。
pub fn enable() -> Result<CodexUnifiedStorageOutcome, AppError> {
    let mut outcome = status();
    let codex_dir = crate::codex_config::get_codex_config_dir();
    // 先做同根校验，任何迁移/建链发生前就拒绝，绝不动真实会话数据。
    ensure_same_home_root(&codex_dir)?;
    fs::create_dir_all(&codex_dir).map_err(|e| AppError::io(&codex_dir, e))?;

    // 状态库最可能被桌面端占用，先迁它：失败时尚未动 sessions，整体可干净重试。
    outcome.migrated_state_dbs = migrate_state_db(&codex_dir)?;
    outcome.migrated_sessions = migrate_dir_to_unified(&codex_dir, "sessions")?;
    outcome.migrated_archived = migrate_dir_to_unified(&codex_dir, "archived_sessions")?;

    create_dir_link(&codex_dir.join("sessions"), &sessions_dir())?;
    create_dir_link(&codex_dir.join("archived_sessions"), &archived_dir())?;

    let marker = unified_home().join(CODEX_UNIFIED_STORAGE_MARKER);
    atomic_write(&marker, b"active\n")?;
    crate::settings::set_unify_codex_session_storage(true)?;

    outcome.enabled = true;
    outcome.active = true;
    outcome.skipped_reason = None;
    Ok(outcome)
}

/// 关闭统一存储：先把数据移回 `~/.codex`，再撤链接、清标记、关开关。
/// 数据移回失败时不改开关状态，避免留下不一致的布局。
pub fn disable() -> Result<CodexUnifiedStorageOutcome, AppError> {
    let mut outcome = status();
    let codex_dir = crate::codex_config::get_codex_config_dir();
    fs::create_dir_all(&codex_dir).map_err(|e| AppError::io(&codex_dir, e))?;

    outcome.migrated_sessions = restore_dir_from_unified(&codex_dir, "sessions")?;
    outcome.migrated_archived = restore_dir_from_unified(&codex_dir, "archived_sessions")?;
    outcome.migrated_state_dbs = restore_state_db(&codex_dir)?;

    let marker = unified_home().join(CODEX_UNIFIED_STORAGE_MARKER);
    let _ = fs::remove_file(&marker);
    crate::settings::set_unify_codex_session_storage(false)?;

    outcome.enabled = false;
    outcome.active = false;
    outcome.skipped_reason = None;
    Ok(outcome)
}

/// 启动时幂等自愈：开关开着但链接丢失（例如用户误删）时重建链接，
/// 并把残留的 `~/.codex` 会话目录迁入统一目录。
pub fn ensure_active_on_startup() -> Result<CodexUnifiedStorageOutcome, AppError> {
    if !is_enabled() {
        let mut outcome = status();
        outcome.skipped_reason = Some("toggle_off".to_string());
        return Ok(outcome);
    }
    enable()
}

// ---------------------------------------------------------------------------
// 目录链接（junction / symlink）
// ---------------------------------------------------------------------------

fn is_dir_link(path: &Path) -> bool {
    #[cfg(windows)]
    {
        // Windows junction 是 reparse point（mount point tag），std 的
        // is_symlink() 不一定把它当 symlink；直接用属性位判断最稳。
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        fs::symlink_metadata(path)
            .map(|meta| (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
    }
}

/// 读取目录链接（junction / symlink）当前指向的目标；非链接或读取失败返回 None。
fn link_target(path: &Path) -> Option<PathBuf> {
    fs::read_link(path).ok()
}

fn create_dir_link(link: &Path, target: &Path) -> Result<(), AppError> {
    if is_dir_link(link) {
        // 自愈：链接目标与期望统一目录不一致时（例如残留测试环境把链接指到
        // 临时目录，或统一目录被手动移动过）撤掉重建，避免 Codex 读写错位。
        if let Some(current) = link_target(link) {
            if normalized_path_key(&current) != normalized_path_key(target) {
                log::warn!(
                    "重建目录链接 {}：当前指向 {}，期望 {}",
                    link.display(),
                    current.display(),
                    target.display()
                );
                remove_dir_link(link)?;
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        }
    }
    if link.exists() {
        return Err(AppError::Message(format!(
            "{} 已存在且不是目录链接，无法统一存放（请先处理该目录）",
            link.display()
        )));
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    if !target.exists() {
        fs::create_dir_all(target).map_err(|e| AppError::io(target, e))?;
    }

    #[cfg(windows)]
    {
        use std::process::Command;
        let link_str = link.display().to_string();
        let target_str = target.display().to_string();
        let output = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                link_str.as_str(),
                target_str.as_str(),
            ])
            .output()
            .map_err(|e| {
                AppError::Message(format!("创建目录链接失败（mklink）: {e}"))
            })?;
        if !output.status.success() {
            return Err(AppError::Message(format!(
                "创建目录链接失败: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).map_err(|e| AppError::io(link, e))
    }
}

fn remove_dir_link(link: &Path) -> Result<(), AppError> {
    if !is_dir_link(link) {
        return Ok(());
    }
    #[cfg(windows)]
    {
        // Windows junction：remove_dir 删除链接本身，不进入目标目录。
        fs::remove_dir(link).map_err(|e| AppError::io(link, e))
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(link).map_err(|e| AppError::io(link, e))
    }
}

// ---------------------------------------------------------------------------
// 迁移
// ---------------------------------------------------------------------------

/// 把 `~/.codex/<name>`（真实目录）移动到统一目录（若统一目录尚空）。
fn migrate_dir_to_unified(codex_dir: &Path, name: &str) -> Result<usize, AppError> {
    let src = codex_dir.join(name);
    let dest = unified_home().join(name);
    if is_dir_link(&src) {
        return Ok(0);
    }
    if !src.exists() {
        return Ok(0);
    }
    if dest.exists() && !is_empty_dir(&dest) {
        // 统一目录已有数据（理论上是上次启用后残留）：不再重复迁移。
        return Err(AppError::Message(format!(
            "统一目录 {} 已存在数据且 ~/.codex/{} 仍是真实目录，无法自动迁移（避免混写数据），请先手动处理",
            dest.display(),
            name
        )));
    }
    fs::create_dir_all(&dest).map_err(|e| AppError::io(&dest, e))?;
    if is_empty_dir(&dest) {
        let _ = fs::remove_dir(&dest);
    }
    let file_count = count_files_recursive(&src);
    if fs::rename(&src, &dest).is_ok() {
        return Ok(file_count);
    }
    // 跨卷或占用时回退复制+删除。
    copy_tree(&src, &dest)?;
    if let Err(e) = remove_tree(&src) {
        log::warn!(
            "迁移 {} 复制完成但删除源失败（重复数据无碍）: {e}",
            src.display()
        );
    }
    Ok(file_count)
}

/// 关闭时把统一目录的数据移回 `~/.codex/<name>`（先撤链接）。
fn restore_dir_from_unified(codex_dir: &Path, name: &str) -> Result<usize, AppError> {
    let link = codex_dir.join(name);
    let unified = unified_home().join(name);
    remove_dir_link(&link)?;

    if !unified.exists() || is_empty_dir(&unified) {
        if unified.exists() {
            let _ = fs::remove_dir(&unified);
        }
        return Ok(0);
    }
    if link.exists() {
        // 链接撤掉后 Codex 可能已经新建了空目录。
        if !is_empty_dir(&link) {
            return Err(AppError::Message(format!(
                "{} 已有新数据，无法把统一目录数据移回（避免覆盖）",
                link.display()
            )));
        }
        let _ = fs::remove_dir(&link);
    }
    let file_count = count_files_recursive(&unified);
    if fs::rename(&unified, &link).is_ok() {
        return Ok(file_count);
    }
    copy_tree(&unified, &link)?;
    if let Err(e) = remove_tree(&unified) {
        log::warn!(
            "移回 {} 复制完成但删除统一副本失败（重复数据无碍）: {e}",
            unified.display()
        );
    }
    Ok(file_count)
}

fn migrate_state_db(codex_dir: &Path) -> Result<usize, AppError> {
    let src = codex_dir.join(CODEX_STATE_DB_FILENAME);
    if !src.exists() {
        return Ok(0);
    }
    let dest = state_db_path();
    if dest.exists() {
        return Ok(0);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    if fs::rename(&src, &dest).is_ok() {
        return Ok(1);
    }
    // 桌面端可能正占用 DB：用 SQLite backup API 做一致性复制。
    if crate::codex_desktop_conversations::snapshot_state_db(&src, &dest) {
        let _ = fs::remove_file(&src);
        return Ok(1);
    }
    Err(AppError::Message(format!(
        "状态库 {} 被占用，迁移失败（请先退出 ChatGPT/Codex 桌面端）",
        src.display()
    )))
}

fn restore_state_db(codex_dir: &Path) -> Result<usize, AppError> {
    let src = state_db_path();
    if !src.exists() {
        return Ok(0);
    }
    let dest = codex_dir.join(CODEX_STATE_DB_FILENAME);
    if dest.exists() {
        return Ok(0);
    }
    if fs::rename(&src, &dest).is_ok() {
        return Ok(1);
    }
    if crate::codex_desktop_conversations::snapshot_state_db(&src, &dest) {
        let _ = fs::remove_file(&src);
        return Ok(1);
    }
    Err(AppError::Message(format!(
        "状态库 {} 被占用，移回失败（请先退出 ChatGPT/Codex 桌面端）",
        src.display()
    )))
}

// ---------------------------------------------------------------------------
// 文件工具
// ---------------------------------------------------------------------------

fn count_files_recursive(dir: &Path) -> usize {
    let mut files = Vec::new();
    collect_files_recursive(dir, &mut files, 0, 24);
    files.len()
}

fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>, depth: u8, max_depth: u8) {
    if depth > max_depth || !dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, files, depth + 1, max_depth);
        } else {
            files.push(path);
        }
    }
}

fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

fn copy_tree(src: &Path, dest: &Path) -> Result<(), AppError> {
    if src.is_dir() {
        fs::create_dir_all(dest).map_err(|e| AppError::io(dest, e))?;
        let entries = fs::read_dir(src).map_err(|e| AppError::io(src, e))?;
        for entry in entries.flatten() {
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }
        copy_file(src, dest)
    }
}

fn remove_tree(path: &Path) -> Result<(), AppError> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|e| AppError::io(path, e))?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|e| AppError::io(path, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("temp dir");
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        std::env::set_var("HOME", temp.path());
        #[cfg(windows)]
        std::env::set_var("USERPROFILE", temp.path());
        crate::settings::set_unify_codex_session_storage(false)
            .expect("reset unified storage toggle");
        let codex_dir = crate::codex_config::get_codex_config_dir();
        fs::create_dir_all(codex_dir.join("sessions/2026/08/13"))
            .expect("create sessions");
        fs::write(
            codex_dir
                .join("sessions/2026/08/13/rollout.jsonl"),
            "{\"type\":\"session_meta\"}\n",
        )
        .expect("write session");
        fs::create_dir_all(codex_dir.join("archived_sessions")).expect("create archived");
        fs::write(
            codex_dir.join("archived_sessions/old.jsonl"),
            "{}",
        )
        .expect("write archived");
        let conn = rusqlite::Connection::open(codex_dir.join(CODEX_STATE_DB_FILENAME))
            .expect("open db");
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY); INSERT INTO threads VALUES ('t1');",
        )
        .expect("seed db");
        (temp, codex_dir)
    }

    fn teardown() {
        std::env::remove_var("CC_SWITCH_TEST_HOME");
        std::env::remove_var("HOME");
        #[cfg(windows)]
        std::env::remove_var("USERPROFILE");
    }

    #[test]
    fn enable_migrates_and_creates_links() {
        let (_temp, codex_dir) = setup();
        let outcome = enable().expect("enable");
        assert!(outcome.enabled);
        assert!(outcome.active);
        assert!(outcome.migrated_sessions >= 1);
        assert!(outcome.migrated_archived >= 1);
        assert_eq!(outcome.migrated_state_dbs, 1);

        // 数据已落到统一目录，~/.codex 下只剩链接。
        assert!(sessions_dir().join("2026/08/13/rollout.jsonl").exists());
        assert!(archived_dir().join("old.jsonl").exists());
        assert!(state_db_path().exists());
        assert!(is_dir_link(&codex_dir.join("sessions")));
        assert!(is_dir_link(&codex_dir.join("archived_sessions")));
        assert!(!codex_dir.join("state_5.sqlite").exists());
        teardown();
    }

    #[test]
    fn enable_is_idempotent() {
        let (_temp, _codex_dir) = setup();
        let first = enable().expect("first enable");
        let second = enable().expect("second enable");
        assert_eq!(second.migrated_sessions, 0);
        assert_eq!(second.migrated_archived, 0);
        assert_eq!(second.migrated_state_dbs, 0);
        assert!(first.active && second.active);
        teardown();
    }

    #[test]
    fn disable_restores_original_layout() {
        let (_temp, codex_dir) = setup();
        enable().expect("enable");
        let outcome = disable().expect("disable");
        assert!(!outcome.enabled);
        assert!(!outcome.active);
        assert!(!is_dir_link(&codex_dir.join("sessions")));
        assert!(codex_dir.join("sessions/2026/08/13/rollout.jsonl").exists());
        assert!(codex_dir.join("archived_sessions/old.jsonl").exists());
        assert!(codex_dir.join("state_5.sqlite").exists());
        teardown();
    }
}
