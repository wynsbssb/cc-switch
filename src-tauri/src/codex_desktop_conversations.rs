//! Codex Desktop 对话数据自动快照 / 恢复。
//!
//! ChatGPT / Codex 桌面端的会话（`originator: "Codex Desktop"`）落在
//! Codex 配置目录（`~/.codex`）里：`sessions/**/*.jsonl`（rollout）、
//! `session_index.jsonl`（列表索引）与 `state_5.sqlite`（线程元数据），以及
//! `archived_sessions/**`（归档）。
//!
//! 用户用 cc-switch 配置第三方供应商后，这些会话归到 `custom` 桶；若随后
//! 关闭 cc-switch、直接在桌面端用 ChatGPT 账号登录，桌面端可能清空/隐藏
//! 本地会话，导致"对话数据丢失"。本模块在 cc-switch 运行期间（启动、切换
//! Codex 供应商、退出、手动触发）把上述数据快照到
//! `~/.cc-switch/backups/codex-desktop-conversations/<时间戳>/`，并提供
//! 一键恢复，保证即使 cc-switch 已退出，对话数据也永远可还原、不真正丢失。
//!
//! 快照是增量的：与上一份快照相比未变化的会话文件用硬链接复用（零数据
//! 写入），只把新增 / 变更的文件真正写盘，避免每次切换 / 退出都重复写
//! 数百 MB 数据、无谓磨损磁盘。
//!
//! 快照与恢复都是 best-effort：任何单项失败只记录日志、不阻断整体流程，
//! 也不会破坏 Codex 配置目录里的既有文件（恢复前会把现状先移入
//! `pre-restore-*` 目录）。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Local;
use rusqlite::backup::Backup;

use crate::codex_config::get_codex_config_dir;
use crate::codex_state_db::codex_state_db_paths;
use crate::config::{atomic_write, get_app_config_dir};
use crate::error::AppError;

/// 快照父目录名（`~/.cc-switch/backups/` 下）。
const CODEX_DESKTOP_CONVERSATIONS_BACKUP_NAME: &str = "codex-desktop-conversations";
/// 恢复前把现状临时移入的前缀目录名。
const CODEX_DESKTOP_CONVERSATIONS_PRE_RESTORE_PREFIX: &str = "pre-restore";
/// 快照写入过程中使用的临时目录前缀；完成后 rename 为正式时间戳目录。
/// 退出路径上的快照有超时上限，进程可能被强杀，临时目录可能残留。
const CODEX_DESKTOP_CONVERSATIONS_IN_PROGRESS_PREFIX: &str = "in-progress";
/// 保留的快照代际数（超出后按时间清理最旧的）。
const CODEX_DESKTOP_CONVERSATIONS_MAX_KEEP: usize = 10;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CodexDesktopConversationsSnapshotOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_dir: Option<String>,
    pub jsonl_files: usize,
    pub archived_files: usize,
    pub state_dbs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CodexDesktopConversationsRestoreOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_dir: Option<String>,
    pub jsonl_files: usize,
    pub archived_files: usize,
    pub state_dbs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

/// 快照父目录：`~/.cc-switch/backups/codex-desktop-conversations`。
fn backup_parent() -> PathBuf {
    get_app_config_dir()
        .join("backups")
        .join(CODEX_DESKTOP_CONVERSATIONS_BACKUP_NAME)
}

/// Codex 配置目录的规范化字符串，用作快照 meta 的目录身份（防止把
/// 另一个 Codex 目录的快照误还原到当前目录）。
fn canonical_dir_string(dir: &Path) -> String {
    fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn timestamp_dir_name(now: chrono::DateTime<Local>) -> String {
    // 毫秒精度：后台快照可能与下一次切换并发，避免同一秒内目录名撞车。
    now.format("%Y%m%d-%H%M%S%.3f").to_string()
}

fn write_snapshot_meta(
    snapshot_dir: &Path,
    codex_dir: &Path,
    outcome: &CodexDesktopConversationsSnapshotOutcome,
    fingerprint: Option<&str>,
) -> Result<(), AppError> {
    let payload = serde_json::json!({
        "codexConfigDir": canonical_dir_string(codex_dir),
        "createdAt": Local::now().to_rfc3339(),
        "jsonlFiles": outcome.jsonl_files,
        "archivedFiles": outcome.archived_files,
        "stateDbs": outcome.state_dbs,
        "fingerprint": fingerprint,
    });
    let bytes =
        serde_json::to_vec_pretty(&payload).map_err(|e| AppError::JsonSerialize { source: e })?;
    atomic_write(&snapshot_dir.join("meta.json"), &bytes)
}

/// 收集某目录下所有文件（保留相对路径），用于递归复制会话目录。
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

fn push_fingerprint_entry(entries: &mut Vec<(String, u64, u64)>, rel: &str, path: &Path) {
    let Ok(meta) = path.metadata() else {
        return;
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    entries.push((rel.to_string(), meta.len(), mtime));
}

fn collect_fingerprint_dir(dir: &Path, prefix: &str, entries: &mut Vec<(String, u64, u64)>) {
    if !dir.is_dir() {
        return;
    }
    let mut files = Vec::new();
    collect_files_recursive(dir, &mut files, 0, 24);
    for file in files {
        let rel = file.strip_prefix(dir).unwrap_or(file.as_path());
        push_fingerprint_entry(
            entries,
            &format!("{prefix}/{}", rel.to_string_lossy()),
            &file,
        );
    }
}

/// 对会话相关文件（相对路径 + 大小 + 修改时间）做 FNV-1a 64 位稳定哈希。
/// 用于跳过"数据未变化"时的重复快照：路由/供应商高频切换不会产生新会话，
/// 不应每次切换都全量复制（可达到数百 MB）。
fn conversation_fingerprint(codex_dir: &Path, config_text: &str) -> Option<String> {
    let mut entries: Vec<(String, u64, u64)> = Vec::new();
    collect_fingerprint_dir(&codex_dir.join("sessions"), "sessions", &mut entries);
    collect_fingerprint_dir(
        &codex_dir.join("archived_sessions"),
        "archived_sessions",
        &mut entries,
    );
    let session_index = codex_dir.join("session_index.jsonl");
    if session_index.exists() {
        push_fingerprint_entry(&mut entries, "session_index.jsonl", &session_index);
    }
    for db in codex_state_db_paths(codex_dir, config_text) {
        if db.exists() {
            let name = db
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            push_fingerprint_entry(&mut entries, &format!("state/{name}"), &db);
        }
    }
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (path, len, mtime) in &entries {
        hash ^= path.len() as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        for b in path.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= *len;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        hash ^= *mtime;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

/// 快照间文件映射使用的相对路径 key（统一 `/` 分隔，跨平台稳定）。
fn snapshot_rel_key(prefix: &str, rel: &Path) -> String {
    format!("{prefix}/{}", rel.to_string_lossy().replace('\\', "/"))
}

/// 收集上一份快照目录的文件（相对路径 key → 大小 + mtime），用于判断
/// 哪些文件可以硬链接复用、避免重复全量写入。
fn collect_snapshot_file_meta(dir: &Path, prefix: &str, map: &mut HashMap<String, (u64, u64)>) {
    if !dir.is_dir() {
        return;
    }
    let mut files = Vec::new();
    collect_files_recursive(dir, &mut files, 0, 24);
    for file in files {
        let Ok(meta) = file.metadata() else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let rel = file.strip_prefix(dir).unwrap_or(file.as_path());
        map.insert(snapshot_rel_key(prefix, rel), (meta.len(), mtime));
    }
}

/// 把 `from_dir` 下的全部文件真实复制到 `to_dir`（保留相对路径结构）。
/// 用于恢复路径：快照 → Codex 目录必须落真实数据，不能复用链接。
/// 返回复制成功的文件数。
fn copy_dir_contents(from_dir: &Path, to_dir: &Path) -> usize {
    if !from_dir.is_dir() {
        return 0;
    }
    let mut files = Vec::new();
    collect_files_recursive(from_dir, &mut files, 0, 24);
    let mut copied = 0;
    for file in files {
        let rel = file.strip_prefix(from_dir).unwrap_or(file.as_path());
        let dest = to_dir.join(rel);
        if let Some(parent) = dest.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log::warn!("创建恢复目录失败 {}: {e}", parent.display());
                continue;
            }
        }
        match crate::config::copy_file(&file, &dest) {
            Ok(()) => copied += 1,
            Err(e) => log::warn!("恢复复制 {} 失败: {e}", file.display()),
        }
    }
    copied
}

/// 复制单个文件到快照；若与上一份快照的同路径文件（大小 + mtime）一致，
/// 则改为硬链接复用——链接源与目标都在备份目录内，必然同卷且零数据写入，
/// 避免每次切换 / 退出都重复把数百 MB 会话数据写一遍。链接失败退回真实复制。
fn copy_or_link_one_file(
    src: &Path,
    dest: &Path,
    prev_dir: Option<&Path>,
    prev_files: &HashMap<String, (u64, u64)>,
    key: &str,
) -> bool {
    if let Some(parent) = dest.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log::warn!("创建快照目录失败 {}: {e}", parent.display());
            return false;
        }
    }
    let Ok(meta) = src.metadata() else {
        log::warn!("读取 {} 元数据失败，跳过", src.display());
        return false;
    };
    let src_mtime = meta.modified().ok();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    if let Some(prev_dir) = prev_dir {
        if prev_files.get(key) == Some(&(meta.len(), mtime)) {
            let prev_path = prev_dir.join(key.replace('/', std::path::MAIN_SEPARATOR_STR));
            if fs::hard_link(&prev_path, dest).is_ok() {
                return true;
            }
            log::debug!(
                "快照硬链接复用 {} 失败，退回复制（可能跨卷）",
                prev_path.display()
            );
        }
    }
    match crate::config::copy_file(src, dest) {
        Ok(()) => {
            // `fs::copy` 在 Linux/macOS 上不保留源文件 mtime；显式写回，
            // 否则下一次快照的大小 + mtime 比对永远不相等，硬链接复用失效，
            // 每次都会退回全量复制。
            if let (Some(mtime), Ok(file)) = (src_mtime, fs::File::options().write(true).open(dest))
            {
                let _ = file.set_modified(mtime);
            }
            true
        }
        Err(e) => {
            log::warn!("快照复制 {} 失败: {e}", src.display());
            false
        }
    }
}

/// 把 `from_dir` 下的全部文件放到 `to_dir`（保留相对路径结构），未变化的
/// 文件优先硬链接复用上一份快照。返回成功写入（复制或链接）的文件数。
fn copy_or_link_dir_contents(
    from_dir: &Path,
    to_dir: &Path,
    prev_dir: Option<&Path>,
    prev_files: &HashMap<String, (u64, u64)>,
    prefix: &str,
) -> usize {
    if !from_dir.is_dir() {
        return 0;
    }
    let mut files = Vec::new();
    collect_files_recursive(from_dir, &mut files, 0, 24);
    let mut copied = 0;
    for file in files {
        let rel = file.strip_prefix(from_dir).unwrap_or(file.as_path());
        let key = snapshot_rel_key(prefix, rel);
        let dest = to_dir.join(rel);
        if copy_or_link_one_file(&file, &dest, prev_dir, prev_files, &key) {
            copied += 1;
        }
    }
    copied
}

/// 用 SQLite backup API 把 `state_5.sqlite`（可能正被桌面端以 WAL 模式占用）
/// 一致性快照到目标路径。返回是否成功。
pub(crate) fn snapshot_state_db(src: &Path, dest: &Path) -> bool {
    if !src.exists() {
        return false;
    }
    if let Some(parent) = dest.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log::warn!("创建 state DB 快照目录失败 {}: {e}", parent.display());
            return false;
        }
    }
    // 目标已存在时 Backup 要求为空数据库；先删掉残留。
    let _ = fs::remove_file(dest);
    let result = (|| -> Result<(), AppError> {
        let src_conn =
            rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| AppError::Message(format!("打开 {} 失败: {e}", src.display())))?;
        let mut dest_conn = rusqlite::Connection::open(dest)
            .map_err(|e| AppError::Message(format!("创建 {} 失败: {e}", dest.display())))?;
        let backup = Backup::new(&src_conn, &mut dest_conn)
            .map_err(|e| AppError::Message(format!("初始化快照失败: {e}")))?;
        // 每批复制 1024 页、只睡 1ms：旧实现每 5 页就睡 250ms，1.25MB 的
        // state DB 也要等约 4 秒，纯属无谓等待；这里一次性拷完，几十毫秒内返回。
        backup
            .run_to_completion(1024, std::time::Duration::from_millis(1), None)
            .map_err(|e| AppError::Message(format!("快照 {} 失败: {e}", src.display())))?;
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(e) => {
            log::warn!(
                "快照 state DB {} 失败（跳过，不阻塞整体）: {e}",
                src.display()
            );
            let _ = fs::remove_file(dest);
            false
        }
    }
}

/// 是否存在当前 Codex 目录的对话快照。
pub fn has_codex_desktop_conversations_backup() -> bool {
    latest_snapshot_dir_for_current_codex_dir().is_some()
}

/// 当前 Codex 目录下最新一份快照目录。
fn latest_snapshot_dir_for_current_codex_dir() -> Option<PathBuf> {
    let codex_dir = get_codex_config_dir();
    latest_snapshot_dir_for_codex_dir(&backup_parent(), &codex_dir)
}

fn latest_snapshot_dir_for_codex_dir(backup_parent: &Path, codex_dir: &Path) -> Option<PathBuf> {
    let codex_key = canonical_dir_string(codex_dir);
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(entries) = fs::read_dir(backup_parent) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(CODEX_DESKTOP_CONVERSATIONS_PRE_RESTORE_PREFIX) {
            continue;
        }
        let meta_path = path.join("meta.json");
        let Ok(meta_text) = fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_text) else {
            continue;
        };
        if meta.get("codexConfigDir").and_then(|v| v.as_str()) != Some(codex_key.as_str()) {
            continue;
        }
        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((modified, path));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, path)| path)
}

/// 清理超出保留数量的旧快照（按目录修改时间倒序保留最新 N 份）。
fn prune_old_snapshots() {
    let parent = backup_parent();
    let Ok(entries) = fs::read_dir(&parent) else {
        return;
    };
    let now = std::time::SystemTime::now();
    // 宽限期：正常快照（启动 / 切换 / 手动）可能耗时较长，只清理明显残留的
    // 中断快照（例如退出超时被强杀时留下的 in-progress 目录）。
    let stale_grace = std::time::Duration::from_secs(10 * 60);
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(CODEX_DESKTOP_CONVERSATIONS_PRE_RESTORE_PREFIX) {
            continue;
        }
        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if name.starts_with(CODEX_DESKTOP_CONVERSATIONS_IN_PROGRESS_PREFIX) {
            // 进程被强杀（退出快照超时等）留下的半成品目录：超过宽限期直接清理，
            // 不参与代际计数，避免把垃圾目录算作有效快照。
            if now.duration_since(modified).unwrap_or_default() > stale_grace {
                if let Err(e) = fs::remove_dir_all(&path) {
                    log::warn!("清理中断快照 {} 失败: {e}", path.display());
                } else {
                    log::info!("已清理中断快照 {}", path.display());
                }
            }
            continue;
        }
        dirs.push((modified, path));
    }
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in dirs.into_iter().skip(CODEX_DESKTOP_CONVERSATIONS_MAX_KEEP) {
        if let Err(e) = fs::remove_dir_all(&path) {
            log::warn!("清理旧对话快照 {} 失败: {e}", path.display());
        } else {
            log::info!("已清理旧对话快照 {}", path.display());
        }
    }
}

/// 快照 Codex 桌面端对话数据（带开关门控）。
///
/// 开关关闭、Codex 目录不存在或没有会话数据时返回 `skipped_reason`，
/// 不会创建空快照。任何单项复制失败都不阻断整体。
pub fn snapshot_codex_desktop_conversations(
) -> Result<CodexDesktopConversationsSnapshotOutcome, AppError> {
    let mut outcome = CodexDesktopConversationsSnapshotOutcome::default();
    if !crate::settings::backup_codex_desktop_conversations() {
        outcome.skipped_reason = Some("toggle_off".to_string());
        return Ok(outcome);
    }
    snapshot_codex_desktop_conversations_into(&get_codex_config_dir(), &backup_parent())
}

/// 快照的实际实现：显式注入 codex 目录与快照父目录，便于测试隔离真实
/// 用户数据。由 `snapshot_codex_desktop_conversations`（带开关门控）调用。
fn snapshot_codex_desktop_conversations_into(
    codex_dir: &Path,
    backup_parent: &Path,
) -> Result<CodexDesktopConversationsSnapshotOutcome, AppError> {
    let mut outcome = CodexDesktopConversationsSnapshotOutcome::default();
    if !codex_dir.is_dir() {
        outcome.skipped_reason = Some("no_codex_dir".to_string());
        return Ok(outcome);
    }

    let sessions_dir = codex_dir.join("sessions");
    let archived_dir = codex_dir.join("archived_sessions");
    let session_index = codex_dir.join("session_index.jsonl");
    let config_text = crate::codex_config::read_codex_config_text().unwrap_or_default();
    let state_dbs: Vec<PathBuf> = codex_state_db_paths(codex_dir, &config_text)
        .into_iter()
        .filter(|p| p.exists())
        .collect();

    let has_any = sessions_dir.is_dir()
        || archived_dir.is_dir()
        || session_index.exists()
        || !state_dbs.is_empty();
    if !has_any {
        outcome.skipped_reason = Some("no_conversation_data".to_string());
        return Ok(outcome);
    }

    // 上一份已完成的快照（带 meta.json 的正式目录）：未变化的会话文件用它
    // 做硬链接复用，只有新增 / 变更的文件才真正写盘。
    let prev_snapshot = latest_snapshot_dir_for_codex_dir(backup_parent, codex_dir);
    let mut prev_files: HashMap<String, (u64, u64)> = HashMap::new();
    if let Some(prev) = &prev_snapshot {
        collect_snapshot_file_meta(&prev.join("sessions"), "sessions", &mut prev_files);
        collect_snapshot_file_meta(
            &prev.join("archived_sessions"),
            "archived_sessions",
            &mut prev_files,
        );
        if let Ok(meta) = prev.join("session_index.jsonl").metadata() {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            prev_files.insert("session_index.jsonl".to_string(), (meta.len(), mtime));
        }
        collect_snapshot_file_meta(&prev.join("state"), "state", &mut prev_files);
    }

    // 先写入临时目录，全部内容（含 meta.json）就绪后再原子 rename 成最终
    // 时间戳目录。退出路径的快照带时限，进程可能被强杀：这样不会留下一个
    // 没有 meta.json、会被 `latest_snapshot_dir_for_codex_dir` 忽略的
    // 半成品快照目录；残留的 in-progress 目录由 prune 按时间清理。
    let ts = timestamp_dir_name(Local::now());
    let staging_dir = backup_parent.join(format!(
        "{CODEX_DESKTOP_CONVERSATIONS_IN_PROGRESS_PREFIX}-{ts}"
    ));
    let snapshot_dir = backup_parent.join(ts);
    if let Err(e) = fs::create_dir_all(&staging_dir) {
        return Err(AppError::io(&staging_dir, e));
    }

    outcome.jsonl_files = copy_or_link_dir_contents(
        &sessions_dir,
        &staging_dir.join("sessions"),
        prev_snapshot.as_deref(),
        &prev_files,
        "sessions",
    );
    outcome.archived_files = copy_or_link_dir_contents(
        &archived_dir,
        &staging_dir.join("archived_sessions"),
        prev_snapshot.as_deref(),
        &prev_files,
        "archived_sessions",
    );
    if session_index.exists() {
        if copy_or_link_one_file(
            &session_index,
            &staging_dir.join("session_index.jsonl"),
            prev_snapshot.as_deref(),
            &prev_files,
            "session_index.jsonl",
        ) {
            outcome.jsonl_files += 1;
        }
    }
    for db in state_dbs {
        let file_name = db.file_name().unwrap_or_default();
        if snapshot_state_db(&db, &staging_dir.join("state").join(file_name)) {
            outcome.state_dbs += 1;
        }
    }

    if outcome.jsonl_files == 0 && outcome.archived_files == 0 && outcome.state_dbs == 0 {
        // 没有实际内容被复制，删掉空目录，避免产生无意义的代际。
        let _ = fs::remove_dir_all(&staging_dir);
        outcome.skipped_reason = Some("no_conversation_data".to_string());
        return Ok(outcome);
    }

    let fingerprint = conversation_fingerprint(codex_dir, &config_text);
    if let Err(e) = write_snapshot_meta(&staging_dir, codex_dir, &outcome, fingerprint.as_deref()) {
        log::warn!("写入对话快照 meta 失败: {e}");
    }

    if let Err(e) = fs::rename(&staging_dir, &snapshot_dir) {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(AppError::io(&snapshot_dir, e));
    }

    outcome.snapshot_dir = Some(snapshot_dir.display().to_string());
    log::info!(
        "✓ Codex 桌面端对话数据已快照: dir={}, jsonl={}, archived={}, state_dbs={}",
        snapshot_dir.display(),
        outcome.jsonl_files,
        outcome.archived_files,
        outcome.state_dbs
    );
    prune_old_snapshots();
    Ok(outcome)
}

/// 当前 Codex 目录对应的最新快照中记录的指纹。
fn latest_snapshot_fingerprint_for(codex_dir: &Path) -> Option<String> {
    let snapshot_dir = latest_snapshot_dir_for_codex_dir(&backup_parent(), codex_dir)?;
    let meta_text = fs::read_to_string(snapshot_dir.join("meta.json")).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta_text).ok()?;
    meta.get("fingerprint")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// 快照 Codex 桌面端对话数据；若自上次快照以来数据未变化则跳过（返回
/// `skipped_reason = "unchanged"`）。用于路由/供应商高频切换场景，
/// 避免每次切换都全量复制数百 MB 的会话文件。
pub fn snapshot_codex_desktop_conversations_if_changed(
) -> Result<CodexDesktopConversationsSnapshotOutcome, AppError> {
    let mut outcome = CodexDesktopConversationsSnapshotOutcome::default();
    if !crate::settings::backup_codex_desktop_conversations() {
        outcome.skipped_reason = Some("toggle_off".to_string());
        return Ok(outcome);
    }

    let codex_dir = get_codex_config_dir();
    if !codex_dir.is_dir() {
        outcome.skipped_reason = Some("no_codex_dir".to_string());
        return Ok(outcome);
    }

    let config_text = crate::codex_config::read_codex_config_text().unwrap_or_default();
    let Some(fingerprint) = conversation_fingerprint(&codex_dir, &config_text) else {
        outcome.skipped_reason = Some("no_conversation_data".to_string());
        return Ok(outcome);
    };

    if latest_snapshot_fingerprint_for(&codex_dir).as_deref() == Some(fingerprint.as_str()) {
        outcome.skipped_reason = Some("unchanged".to_string());
        return Ok(outcome);
    }

    snapshot_codex_desktop_conversations_into(&codex_dir, &backup_parent())
}

/// 把当前 Codex 目录的会话数据（将被覆盖的部分）移到 `pre-restore-<ts>` 目录，
/// 保证恢复操作可回退、绝不真正丢数据。
fn move_current_conversations_to_pre_restore(
    codex_dir: &Path,
    pre_dir: &Path,
) -> Result<(), AppError> {
    fs::create_dir_all(pre_dir).map_err(|e| AppError::io(pre_dir, e))?;
    let names = ["sessions", "archived_sessions", "session_index.jsonl"];
    for name in names {
        let src = codex_dir.join(name);
        if src.exists() {
            let dest = pre_dir.join(name);
            match fs::rename(&src, &dest) {
                Ok(()) => {}
                Err(e) => {
                    // 跨卷/占用时 rename 失败，退回复制+删除。
                    log::warn!(
                        "移动 {} 到恢复前目录失败({e})，改用复制+删除",
                        src.display()
                    );
                    if let Err(copy_err) = copy_tree(&src, &dest) {
                        log::warn!("恢复前复制 {} 失败: {copy_err}", src.display());
                    } else if let Err(del_err) = remove_tree(&src) {
                        log::warn!("恢复前删除 {} 失败: {del_err}", src.display());
                    }
                }
            }
        }
    }
    // state DB 可能正被桌面端占用，rename 会失败；用 SQLite backup 复制。
    let config_text = crate::codex_config::read_codex_config_text().unwrap_or_default();
    for db in codex_state_db_paths(codex_dir, &config_text) {
        if !db.exists() {
            continue;
        }
        let file_name = db.file_name().unwrap_or_default();
        snapshot_state_db(&db, &pre_dir.join("state").join(file_name));
    }
    Ok(())
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
        crate::config::copy_file(src, dest)
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

/// 从当前 Codex 目录对应的最新快照恢复对话数据。
///
/// 恢复前先把现状移到 `pre-restore-<ts>`，因此恢复操作可逆。若桌面端正
/// 在运行导致 state DB 被占用，对应 DB 会恢复失败并在结果中体现，JSONL
/// 会话文件不受影响。
pub fn restore_codex_desktop_conversations(
) -> Result<CodexDesktopConversationsRestoreOutcome, AppError> {
    let codex_dir = get_codex_config_dir();
    let Some(snapshot_dir) = latest_snapshot_dir_for_current_codex_dir() else {
        return Ok(CodexDesktopConversationsRestoreOutcome {
            skipped_reason: Some("no_backup".to_string()),
            ..Default::default()
        });
    };
    restore_codex_desktop_conversations_from(&codex_dir, &snapshot_dir, &backup_parent())
}

/// 恢复的实际实现：显式注入 codex 目录、快照目录与备份父目录，便于测试。
fn restore_codex_desktop_conversations_from(
    codex_dir: &Path,
    snapshot_dir: &Path,
    backup_parent: &Path,
) -> Result<CodexDesktopConversationsRestoreOutcome, AppError> {
    let mut outcome = CodexDesktopConversationsRestoreOutcome::default();
    if !codex_dir.is_dir() {
        fs::create_dir_all(codex_dir).map_err(|e| AppError::io(codex_dir, e))?;
    }

    // 恢复前把现状归档，保证可回退。
    let pre_dir = backup_parent.join(format!(
        "{}-{}",
        CODEX_DESKTOP_CONVERSATIONS_PRE_RESTORE_PREFIX,
        timestamp_dir_name(Local::now())
    ));
    if let Err(e) = move_current_conversations_to_pre_restore(codex_dir, &pre_dir) {
        log::warn!("恢复前归档现状失败（继续恢复）: {e}");
    }

    // 会话 rollout 文件。
    outcome.jsonl_files =
        copy_dir_contents(&snapshot_dir.join("sessions"), &codex_dir.join("sessions"));
    // 归档会话。
    outcome.archived_files = copy_dir_contents(
        &snapshot_dir.join("archived_sessions"),
        &codex_dir.join("archived_sessions"),
    );
    // 会话索引。
    let index_src = snapshot_dir.join("session_index.jsonl");
    if index_src.exists() {
        match crate::config::copy_file(&index_src, &codex_dir.join("session_index.jsonl")) {
            Ok(()) => outcome.jsonl_files += 1,
            Err(e) => log::warn!("恢复 session_index.jsonl 失败: {e}"),
        }
    }
    // state DB。
    let state_src = snapshot_dir.join("state");
    if state_src.is_dir() {
        if let Ok(entries) = fs::read_dir(&state_src) {
            for entry in entries.flatten() {
                let file = entry.path();
                if !file.is_file() {
                    continue;
                }
                let file_name = file.file_name().unwrap_or_default();
                let dest = codex_dir.join(file_name);
                match crate::config::copy_file(&file, &dest) {
                    Ok(()) => outcome.state_dbs += 1,
                    Err(e) => log::warn!(
                        "恢复 state DB {} 失败（桌面端可能正在运行，请关闭后重试）: {e}",
                        file.display()
                    ),
                }
            }
        }
    }

    outcome.snapshot_dir = Some(snapshot_dir.display().to_string());
    log::info!(
        "✓ Codex 桌面端对话数据已从快照恢复: dir={}, jsonl={}, archived={}, state_dbs={}",
        snapshot_dir.display(),
        outcome.jsonl_files,
        outcome.archived_files,
        outcome.state_dbs
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().expect("temp dir");
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        let codex_dir = temp.path().join(".codex");
        let backup_parent = temp
            .path()
            .join(".cc-switch")
            .join("backups")
            .join("codex-desktop-conversations");
        fs::create_dir_all(
            codex_dir
                .join("sessions")
                .join("2026")
                .join("08")
                .join("13"),
        )
        .expect("create sessions");
        fs::write(
            codex_dir
                .join("sessions")
                .join("2026")
                .join("08")
                .join("13")
                .join("rollout-test.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"custom\"}}\n",
        )
        .expect("write session");
        fs::write(
            codex_dir.join("session_index.jsonl"),
            "{\"id\":\"abc\",\"thread_name\":\"test\"}\n",
        )
        .expect("write index");
        fs::create_dir_all(codex_dir.join("archived_sessions")).expect("archived dir");
        fs::write(codex_dir.join("archived_sessions").join("old.jsonl"), "{}")
            .expect("write archived");
        let conn = rusqlite::Connection::open(codex_dir.join("state_5.sqlite")).expect("open db");
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY); INSERT INTO threads VALUES ('t1');",
        )
        .expect("seed db");
        (temp, codex_dir, backup_parent)
    }

    #[test]
    fn snapshot_and_restore_round_trip() {
        let (_temp, codex_dir, backup_parent) = setup();
        let outcome = snapshot_codex_desktop_conversations_into(&codex_dir, &backup_parent)
            .expect("snapshot");
        assert!(outcome.skipped_reason.is_none());
        assert!(outcome.jsonl_files >= 2); // session + session_index
        assert_eq!(outcome.archived_files, 1);
        assert_eq!(outcome.state_dbs, 1);
        let snapshot_dir = outcome.snapshot_dir.clone().expect("snapshot dir");

        // 模拟桌面端清空本地会话
        fs::remove_dir_all(codex_dir.join("sessions")).expect("remove sessions");
        fs::remove_file(codex_dir.join("session_index.jsonl")).expect("remove index");
        fs::remove_file(codex_dir.join("state_5.sqlite")).expect("remove db");

        let latest =
            latest_snapshot_dir_for_codex_dir(&backup_parent, &codex_dir).expect("latest snapshot");
        let restored =
            restore_codex_desktop_conversations_from(&codex_dir, &latest, &backup_parent)
                .expect("restore");
        assert!(restored.skipped_reason.is_none());
        assert!(restored.jsonl_files >= 2);
        assert_eq!(restored.archived_files, 1);
        assert_eq!(restored.state_dbs, 1);

        assert!(codex_dir
            .join("sessions")
            .join("2026")
            .join("08")
            .join("13")
            .join("rollout-test.jsonl")
            .exists());
        assert!(codex_dir.join("session_index.jsonl").exists());
        assert!(codex_dir.join("state_5.sqlite").exists());
        assert!(PathBuf::from(snapshot_dir).exists());
        std::env::remove_var("CC_SWITCH_TEST_HOME");
    }

    #[test]
    fn snapshot_skips_when_no_codex_dir() {
        let temp = TempDir::new().expect("temp dir");
        let missing = temp.path().join("missing-codex");
        let backup_parent = temp.path().join("backups");
        let outcome =
            snapshot_codex_desktop_conversations_into(&missing, &backup_parent).expect("snapshot");
        assert_eq!(outcome.skipped_reason.as_deref(), Some("no_codex_dir"));
        std::env::remove_var("CC_SWITCH_TEST_HOME");
    }

    #[test]
    fn snapshot_skips_when_no_conversation_data() {
        let temp = TempDir::new().expect("temp dir");
        let codex_dir = temp.path().join(".codex");
        fs::create_dir_all(&codex_dir).expect("create codex");
        let backup_parent = temp.path().join("backups");
        let outcome = snapshot_codex_desktop_conversations_into(&codex_dir, &backup_parent)
            .expect("snapshot");
        assert_eq!(
            outcome.skipped_reason.as_deref(),
            Some("no_conversation_data")
        );
        std::env::remove_var("CC_SWITCH_TEST_HOME");
    }

    #[test]
    fn latest_snapshot_only_matches_same_codex_dir() {
        let (_temp, codex_dir, backup_parent) = setup();
        snapshot_codex_desktop_conversations_into(&codex_dir, &backup_parent).expect("snapshot");
        let latest = latest_snapshot_dir_for_codex_dir(&backup_parent, &codex_dir).expect("latest");
        assert!(latest.join("meta.json").exists());

        // 另一个目录不应命中。
        let other = codex_dir.parent().unwrap().join("other-codex");
        assert!(latest_snapshot_dir_for_codex_dir(&backup_parent, &other).is_none());
        std::env::remove_var("CC_SWITCH_TEST_HOME");
    }

    #[test]
    fn if_changed_skips_unchanged_and_snapshots_after_change() {
        let (_temp, codex_dir, _backup_parent) = setup();

        // 第一次调用：数据存在且开关默认开启 -> 真实快照。
        let first = snapshot_codex_desktop_conversations_if_changed().expect("first snapshot");
        assert!(first.skipped_reason.is_none());
        assert!(first.snapshot_dir.is_some());

        // 数据未变化 -> 跳过，不产生新快照。
        let second = snapshot_codex_desktop_conversations_if_changed().expect("second snapshot");
        assert_eq!(second.skipped_reason.as_deref(), Some("unchanged"));

        // 修改会话文件 -> 指纹变化 -> 再次快照。
        let session = codex_dir
            .join("sessions")
            .join("2026")
            .join("08")
            .join("13")
            .join("rollout-test.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"custom-2\"}}\n",
        )
        .expect("touch session");
        let third = snapshot_codex_desktop_conversations_if_changed().expect("third snapshot");
        assert!(third.skipped_reason.is_none());
        assert!(third.jsonl_files >= 2);

        std::env::remove_var("CC_SWITCH_TEST_HOME");
    }

    #[test]
    fn snapshot_commits_atomically_without_leaving_staging_dir() {
        let (_temp, codex_dir, backup_parent) = setup();
        let outcome = snapshot_codex_desktop_conversations_into(&codex_dir, &backup_parent)
            .expect("snapshot");
        let snapshot_dir = PathBuf::from(outcome.snapshot_dir.expect("snapshot dir"));
        assert!(snapshot_dir.join("meta.json").exists());

        // 成功落盘后不应残留 in-progress 临时目录。
        let leftovers: Vec<_> = fs::read_dir(&backup_parent)
            .expect("read backup parent")
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(CODEX_DESKTOP_CONVERSATIONS_IN_PROGRESS_PREFIX)
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging dir left behind: {leftovers:?}"
        );
        std::env::remove_var("CC_SWITCH_TEST_HOME");
    }

    #[test]
    fn incremental_snapshot_reuses_unchanged_files_and_copies_deltas() {
        let (_temp, codex_dir, backup_parent) = setup();
        let first = snapshot_codex_desktop_conversations_into(&codex_dir, &backup_parent)
            .expect("first snapshot");
        let first_dir = PathBuf::from(first.snapshot_dir.expect("first snapshot dir"));

        // 新增一个会话文件，模拟数据增量。
        let new_session = codex_dir
            .join("sessions")
            .join("2026")
            .join("08")
            .join("14")
            .join("rollout-new.jsonl");
        fs::create_dir_all(new_session.parent().expect("parent")).expect("create dir");
        fs::write(&new_session, "{\"type\":\"session_meta\",\"payload\":{}}\n")
            .expect("write new session");

        let second = snapshot_codex_desktop_conversations_into(&codex_dir, &backup_parent)
            .expect("second snapshot");
        let second_dir = PathBuf::from(second.snapshot_dir.expect("second snapshot dir"));

        // 新文件被写入，上一份快照保持原样（增量，不覆盖历史）。
        assert!(second_dir
            .join("sessions")
            .join("2026")
            .join("08")
            .join("14")
            .join("rollout-new.jsonl")
            .exists());
        assert!(!first_dir
            .join("sessions")
            .join("2026")
            .join("08")
            .join("14")
            .join("rollout-new.jsonl")
            .exists());

        // 未变化的文件内容一致。
        let old_rel = codex_dir
            .join("sessions")
            .join("2026")
            .join("08")
            .join("13")
            .join("rollout-test.jsonl");
        let old_rel_snapshot = PathBuf::from("sessions/2026/08/13/rollout-test.jsonl");
        assert_eq!(
            fs::read(&old_rel).expect("live old"),
            fs::read(first_dir.join(&old_rel_snapshot)).expect("first old"),
        );
        assert_eq!(
            fs::read(&old_rel).expect("live old"),
            fs::read(second_dir.join(&old_rel_snapshot)).expect("second old"),
        );
        // 复制后目标文件应保留源 mtime，保证下一轮能命中硬链接复用。
        assert_eq!(
            fs::metadata(&old_rel).expect("live meta").modified().ok(),
            fs::metadata(second_dir.join(&old_rel_snapshot))
                .expect("snapshot meta")
                .modified()
                .ok(),
        );

        std::env::remove_var("CC_SWITCH_TEST_HOME");
    }
}
