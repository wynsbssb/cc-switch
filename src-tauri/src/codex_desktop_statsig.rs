//! Codex Desktop model-whitelist cache reconciliation.
//!
//! The Desktop model picker only shows a model when it passes Statsig's
//! `available_models` whitelist gate: the `use_hidden_models` dynamic config
//! (config id `107580212`) lists the official model ids, and with
//! `use_hidden_models: true` any model outside that list is hidden and the UI
//! falls back to showing the raw id as "???/Custom". The Statsig JS SDK caches
//! that config in the WebView2 localStorage LevelDB under
//! `statsig.cached.evaluations.<sdkKey>`.
//!
//! cc-switch injects its provider catalog model ids into that whitelist and
//! "pins" the cache's last-modified timestamps a fixed horizon into the future
//! so the SDK considers the patched config fresh and does not immediately
//! overwrite it. A background worker retries while the Desktop holds the LevelDB
//! lock and renews the pin so the custom ids survive long sessions.
//!
//! This is a best-effort, fail-open integration: absent, locked, or unsupported
//! Desktop storage is logged and never blocks provider switching.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(test))]
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

#[cfg(not(test))]
use once_cell::sync::OnceCell;
use serde_json::{json, Value};
use toml_edit::DocumentMut;

use crate::codex_config::{
    get_codex_model_catalog_path, read_codex_model_catalog_simplified_from_live,
    resolve_cc_switch_catalog_path,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::config::get_home_dir;

/// Statsig dynamic config id whose `value` carries `available_models`,
/// `default_model` and `use_hidden_models`.
const CODEX_DESKTOP_STATSIG_MODELS_CONFIG_ID: &str = "107580212";
/// Marker cc-switch writes next to the models config so it can later remove
/// exactly the ids it injected (leaving user/OpenAI ids untouched).
const CODEX_DESKTOP_STATSIG_OWNED_MODELS_KEY: &str = "cc_switch_injected_available_models";
const CODEX_DESKTOP_STATSIG_CACHE_KEY_MARKER: &str = "statsig.cached.evaluations";
const CODEX_DESKTOP_STATSIG_LAST_MODIFIED_KEY_MARKER: &str =
    "statsig.last_modified_time.evaluations";
const CODEX_DESKTOP_STATSIG_PIN_HORIZON_MILLIS: i64 = 30 * 24 * 60 * 60 * 1_000;
const CODEX_DESKTOP_CACHE_RETRY_INITIAL_DELAY_SECS: u64 = 1;
#[cfg(not(test))]
const CODEX_DESKTOP_CACHE_RETRY_MAX_DELAY_SECS: u64 = 30;
const CODEX_DESKTOP_CACHE_RENEWAL_INTERVAL_SECS: u64 = 7 * 24 * 60 * 60;
static CODEX_DESKTOP_CACHE_SYNC_GENERATION: AtomicU64 = AtomicU64::new(0);
static CODEX_DESKTOP_CACHE_SYNC_LOCK: Mutex<()> = Mutex::new(());
#[cfg(not(test))]
static CODEX_DESKTOP_CACHE_WORKER_SENDER: OnceCell<Sender<CodexDesktopCacheWorkerUpdate>> =
    OnceCell::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexDesktopStatsigWrapperEncoding {
    Utf8,
    Utf16Le,
}

#[cfg(not(test))]
#[derive(Debug, PartialEq, Eq)]
enum CodexDesktopCacheRetryOutcome {
    Synced(usize),
    Discovering(usize),
    Superseded,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexDesktopCachePathSyncResult {
    updated_count: usize,
    ready_model_cache: bool,
    requires_short_retry: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexDesktopCachePinPolicy {
    Reconcile,
    Preserve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexDesktopCacheSyncResult {
    updated_count: usize,
    needs_discovery: bool,
}

/// Public model ids cc-switch wants the Desktop picker to show: the inline
/// `modelCatalog` mapping (DB SSOT for third-party providers) plus, for the
/// official aggregate provider, the public slots declared in
/// `codexCustomModels`.
fn codex_model_ids_from_settings(settings: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    let push_id = |model: &str, seen: &mut HashSet<String>, ids: &mut Vec<String>| {
        let model = model.trim();
        if !model.is_empty() && seen.insert(model.to_string()) {
            ids.push(model.to_string());
        }
    };

    if let Some(models) = settings
        .get("modelCatalog")
        .and_then(|catalog| catalog.get("models"))
        .and_then(|models| models.as_array())
    {
        for model_config in models {
            if let Some(model) = model_config.get("model").and_then(Value::as_str) {
                push_id(model, &mut seen, &mut ids);
            }
        }
    }

    if let Some(custom_models) = settings.get("codexCustomModels").and_then(Value::as_array) {
        for item in custom_models {
            if let Some(model) = item.get("model").and_then(Value::as_str) {
                push_id(model, &mut seen, &mut ids);
            }
        }
    }

    ids
}

/// Decode a Statsig localStorage wrapper value. Chrome's WebView2 stores the
/// value as either UTF-8 or UTF-16LE text, sometimes with a single 0x00/0x01
/// prefix byte that must be preserved on write.
fn decode_codex_desktop_statsig_wrapper(
    bytes: &[u8],
) -> Option<(Option<u8>, CodexDesktopStatsigWrapperEncoding, Value)> {
    let (prefix, json_bytes) = if matches!(bytes.first(), Some(0) | Some(1)) {
        (bytes.first().copied(), &bytes[1..])
    } else {
        (None, bytes)
    };

    if let Ok(text) = std::str::from_utf8(json_bytes) {
        if let Ok(wrapper) = serde_json::from_str(text) {
            return Some((prefix, CodexDesktopStatsigWrapperEncoding::Utf8, wrapper));
        }
    }

    if json_bytes.len() % 2 == 0 {
        let units = json_bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        if let Ok(text) = String::from_utf16(&units) {
            if let Ok(wrapper) = serde_json::from_str(&text) {
                return Some((prefix, CodexDesktopStatsigWrapperEncoding::Utf16Le, wrapper));
            }
        }
    }

    None
}

fn encode_codex_desktop_statsig_wrapper(
    prefix: Option<u8>,
    encoding: CodexDesktopStatsigWrapperEncoding,
    wrapper: &Value,
) -> Option<Vec<u8>> {
    let text = serde_json::to_string(wrapper).ok()?;
    let mut encoded = Vec::with_capacity(
        match encoding {
            CodexDesktopStatsigWrapperEncoding::Utf8 => text.len(),
            CodexDesktopStatsigWrapperEncoding::Utf16Le => text.len() * 2,
        } + usize::from(prefix.is_some()),
    );
    if let Some(prefix) = prefix {
        encoded.push(prefix);
    }
    match encoding {
        CodexDesktopStatsigWrapperEncoding::Utf8 => encoded.extend_from_slice(text.as_bytes()),
        CodexDesktopStatsigWrapperEncoding::Utf16Le => {
            for unit in text.encode_utf16() {
                encoded.extend_from_slice(&unit.to_le_bytes());
            }
        }
    }
    Some(encoded)
}

fn codex_desktop_statsig_available_model_ids(wrapper: &Value) -> Option<HashSet<String>> {
    let data_text = wrapper.get("data").and_then(Value::as_str)?;
    let data = serde_json::from_str::<Value>(data_text).ok()?;
    data.get("dynamic_configs")
        .and_then(|value| value.get(CODEX_DESKTOP_STATSIG_MODELS_CONFIG_ID))
        .and_then(|value| value.get("value"))
        .and_then(|value| value.get("available_models"))
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
}

fn codex_desktop_statsig_has_only_models_config(wrapper: &Value) -> bool {
    let Some(data_text) = wrapper.get("data").and_then(Value::as_str) else {
        return false;
    };
    let Ok(data) = serde_json::from_str::<Value>(data_text) else {
        return false;
    };
    let Some(dynamic_configs) = data.get("dynamic_configs").and_then(Value::as_object) else {
        return false;
    };
    let has_sibling_evaluations =
        ["feature_gates", "layer_configs"]
            .iter()
            .any(|key| match data.get(*key) {
                None => false,
                Some(Value::Object(entries)) => !entries.is_empty(),
                Some(_) => true,
            });
    !has_sibling_evaluations
        && dynamic_configs.len() == 1
        && dynamic_configs.contains_key(CODEX_DESKTOP_STATSIG_MODELS_CONFIG_ID)
}

fn codex_desktop_statsig_has_all_models(wrapper: &Value, model_ids: &[String]) -> bool {
    let Some(available_models) = codex_desktop_statsig_available_model_ids(wrapper) else {
        return false;
    };
    model_ids
        .iter()
        .all(|model_id| available_models.contains(model_id))
}

fn codex_desktop_leveldb_origin_for_marker(key_text: &str, marker: &str) -> Option<String> {
    let marker_start = key_text.find(marker)?;
    Some(
        key_text[..marker_start]
            .trim_end_matches(char::from(0))
            .to_string(),
    )
}

fn codex_desktop_statsig_cache_key_from_leveldb_key(key_text: &str) -> Option<String> {
    let start = key_text.find(CODEX_DESKTOP_STATSIG_CACHE_KEY_MARKER)?;
    Some(key_text[start..].trim_matches(char::from(0)).to_string())
}

fn codex_desktop_now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

fn pin_codex_desktop_statsig_last_modified_cache_keys(
    last_modified: &mut Value,
    cache_keys: &HashSet<String>,
    now_millis: i64,
) -> bool {
    if cache_keys.is_empty() {
        return false;
    }
    let Some(entries) = last_modified.as_object_mut() else {
        return false;
    };
    let pinned_until = now_millis.saturating_add(CODEX_DESKTOP_STATSIG_PIN_HORIZON_MILLIS);
    let mut changed = false;
    for cache_key in cache_keys {
        if entries.get(cache_key).and_then(Value::as_i64) != Some(pinned_until) {
            entries.insert(cache_key.clone(), json!(pinned_until));
            changed = true;
        }
    }
    changed
}

fn unpin_codex_desktop_statsig_last_modified_cache_keys(
    last_modified: &mut Value,
    cache_keys: &HashSet<String>,
    now_millis: i64,
) -> bool {
    if cache_keys.is_empty() {
        return false;
    }
    let Some(entries) = last_modified.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for cache_key in cache_keys {
        if entries
            .get(cache_key)
            .and_then(Value::as_i64)
            .is_some_and(|value| value > now_millis)
        {
            entries.insert(cache_key.clone(), json!(now_millis));
            changed = true;
        }
    }
    changed
}

fn merge_codex_desktop_statsig_available_models(wrapper: &mut Value, model_ids: &[String]) -> bool {
    let previous_owned = wrapper
        .get(CODEX_DESKTOP_STATSIG_OWNED_MODELS_KEY)
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let Some(data_text) = wrapper.get("data").and_then(Value::as_str) else {
        return false;
    };
    let Ok(mut data) = serde_json::from_str::<Value>(data_text) else {
        return false;
    };
    let Some(data_obj) = data.as_object_mut() else {
        return false;
    };
    let Some(dynamic_configs) = data_obj
        .get_mut("dynamic_configs")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let Some(config) = dynamic_configs
        .get_mut(CODEX_DESKTOP_STATSIG_MODELS_CONFIG_ID)
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let Some(value) = config.get_mut("value").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(available_models) = value
        .get_mut("available_models")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };

    let original_models = available_models.clone();
    available_models.retain(|model| {
        model
            .as_str()
            .map(|model_id| !previous_owned.contains(model_id))
            .unwrap_or(true)
    });
    let mut seen = available_models
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut current_owned = Vec::new();
    for model_id in model_ids {
        if seen.insert(model_id.clone()) {
            available_models.push(json!(model_id));
            current_owned.push(model_id.clone());
        }
    }
    let data_changed = *available_models != original_models;
    let updated_data_text = if data_changed {
        let Ok(updated_data_text) = serde_json::to_string(&data) else {
            return false;
        };
        Some(updated_data_text)
    } else {
        None
    };

    let Some(wrapper_obj) = wrapper.as_object_mut() else {
        return false;
    };
    if let Some(updated_data_text) = updated_data_text {
        wrapper_obj.insert("data".to_string(), json!(updated_data_text));
    }
    let metadata_changed = if current_owned.is_empty() {
        wrapper_obj
            .remove(CODEX_DESKTOP_STATSIG_OWNED_MODELS_KEY)
            .is_some()
    } else {
        let current_owned = json!(current_owned);
        if wrapper_obj.get(CODEX_DESKTOP_STATSIG_OWNED_MODELS_KEY) == Some(&current_owned) {
            false
        } else {
            wrapper_obj.insert(
                CODEX_DESKTOP_STATSIG_OWNED_MODELS_KEY.to_string(),
                current_owned,
            );
            true
        }
    };
    data_changed || metadata_changed
}

fn codex_desktop_leveldb_candidates_for_root(codex_root: &Path) -> Vec<PathBuf> {
    vec![
        codex_root.join("Local Storage").join("leveldb"),
        codex_root
            .join("Default")
            .join("Local Storage")
            .join("leveldb"),
        codex_root
            .join("Partitions")
            .join("codex-browser-app")
            .join("Local Storage")
            .join("leveldb"),
        codex_root
            .join("Default")
            .join("Partitions")
            .join("codex-browser-app")
            .join("Local Storage")
            .join("leveldb"),
    ]
}

#[cfg(target_os = "linux")]
fn codex_desktop_linux_config_root(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: &Path,
) -> PathBuf {
    xdg_config_home
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("Codex")
}

#[cfg(target_os = "windows")]
fn codex_desktop_windows_leveldb_candidates(appdata: &Path, local_appdata: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let appdata_codex = appdata.join("Codex");
    candidates.extend(codex_desktop_leveldb_candidates_for_root(&appdata_codex));
    candidates.extend(codex_desktop_leveldb_candidates_for_root(
        &appdata_codex.join("web").join("Codex"),
    ));

    let packages = local_appdata.join("Packages");
    if let Ok(entries) = std::fs::read_dir(packages) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !name.starts_with("openai.codex_") {
                continue;
            }
            let package_codex = entry
                .path()
                .join("LocalCache")
                .join("Roaming")
                .join("Codex");
            candidates.extend(codex_desktop_leveldb_candidates_for_root(&package_codex));
            candidates.extend(codex_desktop_leveldb_candidates_for_root(
                &package_codex.join("web").join("Codex"),
            ));
        }
    }
    candidates
}

fn codex_desktop_local_storage_leveldb_candidates() -> Vec<PathBuf> {
    #[cfg(test)]
    if let Some(test_home) = std::env::var_os("CC_SWITCH_TEST_HOME") {
        return codex_desktop_leveldb_candidates_for_root(
            &PathBuf::from(test_home).join("Codex Desktop"),
        );
    }

    let mut candidates = Vec::new();

    #[cfg(target_os = "windows")]
    if let (Some(appdata), Some(local_appdata)) = (
        std::env::var_os("APPDATA"),
        std::env::var_os("LOCALAPPDATA"),
    ) {
        candidates.extend(codex_desktop_windows_leveldb_candidates(
            &PathBuf::from(appdata),
            &PathBuf::from(local_appdata),
        ));
    }

    #[cfg(target_os = "macos")]
    {
        let codex_root = get_home_dir()
            .join("Library")
            .join("Application Support")
            .join("Codex");
        candidates.extend(codex_desktop_leveldb_candidates_for_root(&codex_root));
    }

    #[cfg(target_os = "linux")]
    {
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let codex_root =
            codex_desktop_linux_config_root(xdg_config_home.as_deref(), &get_home_dir());
        candidates.extend(codex_desktop_leveldb_candidates_for_root(&codex_root));
    }

    candidates
}

fn sync_codex_desktop_available_models_cache_path_with_mode(
    leveldb_path: &Path,
    model_ids: &[String],
    pin_policy: CodexDesktopCachePinPolicy,
) -> Result<CodexDesktopCachePathSyncResult, String> {
    let options = rusty_leveldb::Options {
        create_if_missing: false,
        ..Default::default()
    };
    let mut db = rusty_leveldb::DB::open(leveldb_path, options).map_err(|err| match err.code {
        rusty_leveldb::StatusCode::LockError => {
            format!("Codex Desktop localStorage LevelDB is locked: {leveldb_path:?}")
        }
        _ => format!("Failed to open Codex Desktop localStorage LevelDB {leveldb_path:?}: {err}"),
    })?;

    let mut cache_entries = Vec::new();
    let mut last_modified_entries = Vec::new();
    {
        use rusty_leveldb::LdbIterator;

        let mut iter = db.new_iter().map_err(|err| {
            format!("Failed to iterate Codex Desktop localStorage LevelDB: {err}")
        })?;
        while iter.advance() {
            let Some((key, value)) = iter.current() else {
                continue;
            };
            let key_text = String::from_utf8_lossy(&key).to_string();
            if key_text.contains(CODEX_DESKTOP_STATSIG_LAST_MODIFIED_KEY_MARKER) {
                if let (Some(origin), Some((prefix, encoding, last_modified))) = (
                    codex_desktop_leveldb_origin_for_marker(
                        &key_text,
                        CODEX_DESKTOP_STATSIG_LAST_MODIFIED_KEY_MARKER,
                    ),
                    decode_codex_desktop_statsig_wrapper(&value),
                ) {
                    last_modified_entries.push((
                        key.to_vec(),
                        origin,
                        prefix,
                        encoding,
                        last_modified,
                    ));
                }
            }
            if !key_text.contains(CODEX_DESKTOP_STATSIG_CACHE_KEY_MARKER) {
                continue;
            }
            cache_entries.push((key.to_vec(), key_text, value.to_vec()));
        }
    }

    let mut updates = Vec::new();
    let mut requires_short_retry = false;
    let mut model_cache_origins = HashSet::new();
    let mut ready_model_cache_origins = HashSet::new();
    let mut pinned_cache_keys_by_origin: HashMap<String, HashSet<String>> = HashMap::new();
    let mut unpinned_cache_keys_by_origin: HashMap<String, HashSet<String>> = HashMap::new();
    let mut model_cache_keys_by_origin: HashMap<String, HashSet<String>> = HashMap::new();
    for (key, key_text, value) in cache_entries {
        let Some(origin) = codex_desktop_leveldb_origin_for_marker(
            &key_text,
            CODEX_DESKTOP_STATSIG_CACHE_KEY_MARKER,
        ) else {
            continue;
        };
        let Some(cache_key) = codex_desktop_statsig_cache_key_from_leveldb_key(&key_text) else {
            continue;
        };
        let Some((prefix, encoding, mut wrapper)) = decode_codex_desktop_statsig_wrapper(&value)
        else {
            continue;
        };
        let has_models_config = codex_desktop_statsig_available_model_ids(&wrapper).is_some();
        let models_config_isolated =
            has_models_config && codex_desktop_statsig_has_only_models_config(&wrapper);
        if has_models_config {
            model_cache_origins.insert(origin.clone());
            model_cache_keys_by_origin
                .entry(origin.clone())
                .or_default()
                .insert(cache_key.clone());
            if !model_ids.is_empty() && !models_config_isolated {
                requires_short_retry = true;
                unpinned_cache_keys_by_origin
                    .entry(origin.clone())
                    .or_default()
                    .insert(cache_key.clone());
            }
        }
        let changed = merge_codex_desktop_statsig_available_models(&mut wrapper, model_ids);
        let has_all_models =
            !model_ids.is_empty() && codex_desktop_statsig_has_all_models(&wrapper, model_ids);
        if changed {
            let Some(updated) = encode_codex_desktop_statsig_wrapper(prefix, encoding, &wrapper)
            else {
                continue;
            };
            updates.push((key, updated));
        }
        if has_all_models && models_config_isolated {
            pinned_cache_keys_by_origin
                .entry(origin)
                .or_default()
                .insert(cache_key);
        }
    }

    let now_millis = codex_desktop_now_millis();
    for (key, origin, prefix, encoding, mut last_modified) in last_modified_entries {
        if pin_policy == CodexDesktopCachePinPolicy::Preserve {
            continue;
        }
        let unpinned_cache_keys = if model_ids.is_empty() {
            model_cache_keys_by_origin.get(&origin)
        } else {
            unpinned_cache_keys_by_origin.get(&origin)
        };
        let pinned_cache_keys = (!model_ids.is_empty())
            .then(|| pinned_cache_keys_by_origin.get(&origin))
            .flatten();
        if unpinned_cache_keys.is_none() && pinned_cache_keys.is_none() {
            continue;
        }
        let mut changed = false;
        if let Some(cache_keys) = unpinned_cache_keys {
            changed |= unpin_codex_desktop_statsig_last_modified_cache_keys(
                &mut last_modified,
                cache_keys,
                now_millis,
            );
        }
        if let Some(cache_keys) = pinned_cache_keys {
            changed |= pin_codex_desktop_statsig_last_modified_cache_keys(
                &mut last_modified,
                cache_keys,
                now_millis,
            );
        }
        let pinned_after_update = pinned_cache_keys.is_some_and(|cache_keys| {
            !cache_keys.is_empty()
                && cache_keys.iter().all(|cache_key| {
                    last_modified
                        .get(cache_key)
                        .and_then(Value::as_i64)
                        .is_some_and(|value| value > now_millis)
                })
        });
        if pinned_after_update {
            ready_model_cache_origins.insert(origin.clone());
        }
        if !changed {
            continue;
        }
        let Some(updated) = encode_codex_desktop_statsig_wrapper(prefix, encoding, &last_modified)
        else {
            continue;
        };
        updates.push((key, updated));
    }

    let updated_count = updates.len();

    let ready_model_cache = !model_cache_origins.is_empty()
        && model_cache_origins
            .iter()
            .all(|origin| ready_model_cache_origins.contains(origin));
    for (key, value) in updates {
        db.put(&key, &value)
            .map_err(|err| format!("Failed to update Codex Desktop localStorage LevelDB: {err}"))?;
    }
    db.close()
        .map_err(|err| format!("Failed to close Codex Desktop localStorage LevelDB: {err}"))?;
    Ok(CodexDesktopCachePathSyncResult {
        updated_count,
        ready_model_cache,
        requires_short_retry,
    })
}

#[cfg(test)]
fn sync_codex_desktop_available_models_cache_path_with_status(
    leveldb_path: &Path,
    model_ids: &[String],
) -> Result<CodexDesktopCachePathSyncResult, String> {
    sync_codex_desktop_available_models_cache_path_with_mode(
        leveldb_path,
        model_ids,
        CodexDesktopCachePinPolicy::Reconcile,
    )
}

fn codex_desktop_leveldb_candidate_is_active_layout_for_platform(
    path: &Path,
    root_layouts_active: bool,
) -> bool {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    components
        .iter()
        .any(|component| component.eq_ignore_ascii_case("Partitions"))
        || components.windows(2).any(|pair| {
            pair[0].eq_ignore_ascii_case("web") && pair[1].eq_ignore_ascii_case("Codex")
        })
        || (root_layouts_active
            && components.windows(2).any(|pair| {
                pair[0].eq_ignore_ascii_case("Local Storage")
                    && pair[1].eq_ignore_ascii_case("leveldb")
            }))
}

fn codex_desktop_leveldb_candidate_is_active_layout(path: &Path) -> bool {
    codex_desktop_leveldb_candidate_is_active_layout_for_platform(
        path,
        cfg!(any(target_os = "macos", target_os = "linux")),
    )
}

fn sync_codex_desktop_available_models_cache_candidates_with_mode(
    candidates: &[PathBuf],
    model_ids: &[String],
    pin_policy: CodexDesktopCachePinPolicy,
) -> Result<CodexDesktopCacheSyncResult, String> {
    let mut seen = HashSet::new();
    let paths = candidates
        .iter()
        .filter(|path| path.exists())
        .filter(|path| seen.insert((*path).clone()))
        .collect::<Vec<_>>();
    let mut updated_count = 0;
    let mut found_active_layout = false;
    let mut all_active_model_caches_ready = true;
    let mut found_active_short_retry = false;
    let mut errors = Vec::new();
    for path in paths {
        match sync_codex_desktop_available_models_cache_path_with_mode(path, model_ids, pin_policy)
        {
            Ok(result) => {
                updated_count += result.updated_count;
                if codex_desktop_leveldb_candidate_is_active_layout(path) {
                    found_active_layout = true;
                    all_active_model_caches_ready &= result.ready_model_cache;
                    found_active_short_retry |= result.requires_short_retry;
                }
            }
            Err(err) => errors.push(err),
        }
    }
    let has_locked_path = errors
        .iter()
        .any(|err| codex_desktop_cache_error_is_locked(err));
    if has_locked_path || (updated_count == 0 && !errors.is_empty()) {
        Err(errors.join("; "))
    } else {
        if !errors.is_empty() {
            log::warn!(
                "Some Codex Desktop model whitelist cache paths could not be synced: {}",
                errors.join("; ")
            );
        }
        Ok(CodexDesktopCacheSyncResult {
            updated_count,
            needs_discovery: !errors.is_empty()
                || (!model_ids.is_empty()
                    && (!found_active_layout
                        || !all_active_model_caches_ready
                        || found_active_short_retry)),
        })
    }
}

fn sync_codex_desktop_available_models_cache_with_mode(
    model_ids: &[String],
    pin_policy: CodexDesktopCachePinPolicy,
) -> Result<CodexDesktopCacheSyncResult, String> {
    sync_codex_desktop_available_models_cache_candidates_with_mode(
        &codex_desktop_local_storage_leveldb_candidates(),
        model_ids,
        pin_policy,
    )
}

fn codex_desktop_cache_error_is_locked(err: &str) -> bool {
    err.contains("Codex Desktop localStorage LevelDB is locked:")
}

#[cfg(not(test))]
fn attempt_codex_desktop_available_models_cache_retry(
    generation: u64,
    model_ids: &[String],
    pin_policy: CodexDesktopCachePinPolicy,
) -> CodexDesktopCacheRetryOutcome {
    let _guard = CODEX_DESKTOP_CACHE_SYNC_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if CODEX_DESKTOP_CACHE_SYNC_GENERATION.load(Ordering::Acquire) != generation {
        return CodexDesktopCacheRetryOutcome::Superseded;
    }
    match sync_codex_desktop_available_models_cache_with_mode(model_ids, pin_policy) {
        Ok(result) if result.needs_discovery => {
            CodexDesktopCacheRetryOutcome::Discovering(result.updated_count)
        }
        Ok(result) => CodexDesktopCacheRetryOutcome::Synced(result.updated_count),
        Err(err) => CodexDesktopCacheRetryOutcome::Failed(err),
    }
}

#[cfg(not(test))]
fn codex_desktop_cache_worker_next_delay(
    outcome: &CodexDesktopCacheRetryOutcome,
    has_custom_catalog: bool,
    retry_delay_secs: &mut u64,
) -> Option<Duration> {
    match outcome {
        CodexDesktopCacheRetryOutcome::Superseded => None,
        CodexDesktopCacheRetryOutcome::Synced(_) if !has_custom_catalog => None,
        CodexDesktopCacheRetryOutcome::Synced(_) => {
            *retry_delay_secs = CODEX_DESKTOP_CACHE_RETRY_INITIAL_DELAY_SECS;
            Some(Duration::from_secs(
                CODEX_DESKTOP_CACHE_RENEWAL_INTERVAL_SECS,
            ))
        }
        CodexDesktopCacheRetryOutcome::Discovering(_)
        | CodexDesktopCacheRetryOutcome::Failed(_) => {
            let delay = Duration::from_secs(*retry_delay_secs);
            *retry_delay_secs = retry_delay_secs
                .saturating_mul(2)
                .min(CODEX_DESKTOP_CACHE_RETRY_MAX_DELAY_SECS);
            Some(delay)
        }
    }
}

#[cfg(not(test))]
struct CodexDesktopCacheWorkerTask {
    generation: u64,
    model_ids: Vec<String>,
    pin_policy: CodexDesktopCachePinPolicy,
    next_delay: Duration,
    retry_delay_secs: u64,
}

#[cfg(not(test))]
enum CodexDesktopCacheWorkerUpdate {
    Schedule {
        generation: u64,
        model_ids: Vec<String>,
        pin_policy: CodexDesktopCachePinPolicy,
        next_delay: Duration,
    },
    Cancel {
        generation: u64,
    },
}

#[cfg(not(test))]
fn run_codex_desktop_available_models_cache_worker(
    receiver: Receiver<CodexDesktopCacheWorkerUpdate>,
) {
    let mut task: Option<CodexDesktopCacheWorkerTask> = None;
    loop {
        let update = match task.as_ref().map(|task| task.next_delay) {
            Some(delay) => match receiver.recv_timeout(delay) {
                Ok(update) => Some(update),
                Err(RecvTimeoutError::Timeout) => {
                    let mut active_task = task.take().expect("cache worker task exists");
                    let outcome = attempt_codex_desktop_available_models_cache_retry(
                        active_task.generation,
                        &active_task.model_ids,
                        active_task.pin_policy,
                    );
                    match &outcome {
                        CodexDesktopCacheRetryOutcome::Synced(updated) if *updated > 0 => {
                            log::debug!(
                                "Synced {updated} Codex Desktop model whitelist cache entries in the background"
                            );
                        }
                        CodexDesktopCacheRetryOutcome::Synced(_) => log::debug!(
                            "Codex Desktop model whitelist cache is synced; waiting for renewal"
                        ),
                        CodexDesktopCacheRetryOutcome::Discovering(updated) => log::debug!(
                            "Synced {updated} legacy Codex Desktop model whitelist cache entries; retrying partition discovery"
                        ),
                        CodexDesktopCacheRetryOutcome::Superseded => log::debug!(
                            "Cancelled stale Codex Desktop model whitelist cache work after a provider change"
                        ),
                        CodexDesktopCacheRetryOutcome::Failed(err) => log::warn!(
                            "Codex Desktop model whitelist cache background sync failed: {err}"
                        ),
                    }
                    if let Some(next_delay) = codex_desktop_cache_worker_next_delay(
                        &outcome,
                        !active_task.model_ids.is_empty(),
                        &mut active_task.retry_delay_secs,
                    ) {
                        active_task.next_delay = next_delay;
                        task = Some(active_task);
                    }
                    None
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match receiver.recv() {
                Ok(update) => Some(update),
                Err(_) => return,
            },
        };

        if let Some(update) = update {
            match update {
                CodexDesktopCacheWorkerUpdate::Schedule {
                    generation,
                    model_ids,
                    pin_policy,
                    next_delay,
                } if CODEX_DESKTOP_CACHE_SYNC_GENERATION.load(Ordering::Acquire) == generation => {
                    task = Some(CodexDesktopCacheWorkerTask {
                        generation,
                        model_ids,
                        pin_policy,
                        next_delay,
                        retry_delay_secs: CODEX_DESKTOP_CACHE_RETRY_INITIAL_DELAY_SECS,
                    });
                }
                CodexDesktopCacheWorkerUpdate::Cancel { generation }
                    if CODEX_DESKTOP_CACHE_SYNC_GENERATION.load(Ordering::Acquire)
                        == generation =>
                {
                    task = None;
                }
                CodexDesktopCacheWorkerUpdate::Schedule { .. }
                | CodexDesktopCacheWorkerUpdate::Cancel { .. } => log::debug!(
                    "Ignored stale Codex Desktop cache worker update after a newer provider change"
                ),
            }
        }
    }
}

#[cfg(not(test))]
fn codex_desktop_available_models_cache_worker_sender(
) -> Result<&'static Sender<CodexDesktopCacheWorkerUpdate>, String> {
    CODEX_DESKTOP_CACHE_WORKER_SENDER.get_or_try_init(
        || -> Result<Sender<CodexDesktopCacheWorkerUpdate>, String> {
            let (sender, receiver) = mpsc::channel();
            std::thread::Builder::new()
                .name("codex-desktop-cache-worker".to_string())
                .spawn(move || run_codex_desktop_available_models_cache_worker(receiver))
                .map_err(|err| format!("Failed to start Codex Desktop cache worker: {err}"))?;
            Ok(sender)
        },
    )
}

#[cfg(not(test))]
fn schedule_codex_desktop_available_models_cache_worker(
    generation: u64,
    model_ids: Vec<String>,
    pin_policy: CodexDesktopCachePinPolicy,
    next_delay: Option<Duration>,
) {
    let update = match next_delay {
        Some(next_delay) => CodexDesktopCacheWorkerUpdate::Schedule {
            generation,
            model_ids,
            pin_policy,
            next_delay,
        },
        None => CodexDesktopCacheWorkerUpdate::Cancel { generation },
    };
    match codex_desktop_available_models_cache_worker_sender() {
        Ok(sender) => {
            if let Err(err) = sender.send(update) {
                log::warn!("Failed to update Codex Desktop cache worker: {err}");
            }
        }
        Err(err) => log::warn!("{err}"),
    }
}

#[cfg(test)]
fn schedule_codex_desktop_available_models_cache_worker(
    _generation: u64,
    _model_ids: Vec<String>,
    _pin_policy: CodexDesktopCachePinPolicy,
    _next_delay: Option<Duration>,
) {
}

fn invalidate_codex_desktop_available_models_cache_worker() {
    let generation = {
        let _guard = CODEX_DESKTOP_CACHE_SYNC_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        CODEX_DESKTOP_CACHE_SYNC_GENERATION
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    };
    schedule_codex_desktop_available_models_cache_worker(
        generation,
        Vec::new(),
        CodexDesktopCachePinPolicy::Reconcile,
        None,
    );
}

pub fn sync_codex_desktop_available_models_cache_from_settings(settings: &Value) {
    sync_codex_desktop_available_models_cache_model_ids(codex_model_ids_from_settings(settings));
}

fn sync_codex_desktop_available_models_cache_model_ids_with_policy(
    model_ids: Vec<String>,
    pin_policy: CodexDesktopCachePinPolicy,
) {
    let (generation, sync_result) = {
        let _guard = CODEX_DESKTOP_CACHE_SYNC_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = CODEX_DESKTOP_CACHE_SYNC_GENERATION
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        (
            generation,
            sync_codex_desktop_available_models_cache_with_mode(&model_ids, pin_policy),
        )
    };
    match sync_result {
        Ok(result) => {
            let updated = result.updated_count;
            if updated > 0 {
                log::info!("Synced {updated} Codex Desktop model whitelist cache entries");
            }
            let next_delay = if result.needs_discovery {
                Some(Duration::from_secs(
                    CODEX_DESKTOP_CACHE_RETRY_INITIAL_DELAY_SECS,
                ))
            } else if model_ids.is_empty() {
                None
            } else {
                Some(Duration::from_secs(
                    CODEX_DESKTOP_CACHE_RENEWAL_INTERVAL_SECS,
                ))
            };
            schedule_codex_desktop_available_models_cache_worker(
                generation, model_ids, pin_policy, next_delay,
            );
        }
        Err(err) if codex_desktop_cache_error_is_locked(&err) => {
            log::warn!(
                "Codex provider switched while the Desktop model whitelist cache was locked; retrying in the background: {err}"
            );
            schedule_codex_desktop_available_models_cache_worker(
                generation,
                model_ids,
                pin_policy,
                Some(Duration::from_secs(
                    CODEX_DESKTOP_CACHE_RETRY_INITIAL_DELAY_SECS,
                )),
            );
        }
        Err(err) => {
            log::warn!(
                "Codex provider switched, but the Desktop model whitelist cache was not synced: {err}"
            );
            let next_delay = Some(Duration::from_secs(
                CODEX_DESKTOP_CACHE_RETRY_INITIAL_DELAY_SECS,
            ));
            schedule_codex_desktop_available_models_cache_worker(
                generation, model_ids, pin_policy, next_delay,
            );
        }
    }
}

fn sync_codex_desktop_available_models_cache_model_ids(model_ids: Vec<String>) {
    sync_codex_desktop_available_models_cache_model_ids_with_policy(
        model_ids,
        CodexDesktopCachePinPolicy::Reconcile,
    );
}

fn remove_codex_desktop_owned_models_preserving_cache_pins() {
    sync_codex_desktop_available_models_cache_model_ids_with_policy(
        Vec::new(),
        CodexDesktopCachePinPolicy::Preserve,
    );
}

/// Whether `config_text` points `model_catalog_json` at a file cc-switch does
/// not own (different filename or a path outside the config dir). Such an
/// external catalog is the user's own; cc-switch must not inject ids into the
/// Desktop whitelist for it.
fn codex_config_has_external_model_catalog_pointer(
    config_text: &str,
    generated_path: &Path,
) -> bool {
    let Ok(doc) = config_text.parse::<DocumentMut>() else {
        return false;
    };
    let has_pointer = doc
        .get("model_catalog_json")
        .and_then(|item| item.as_str())
        .is_some_and(|path| !path.trim().is_empty());
    has_pointer && resolve_cc_switch_catalog_path(config_text, generated_path).is_none()
}

fn codex_desktop_available_models_cache_ids_after_restored_catalog_reload(
    settings: &Value,
    model_catalog: Option<Value>,
) -> Option<Vec<String>> {
    let model_catalog = model_catalog?;
    let mut restored_settings = settings.clone();
    let object = restored_settings.as_object_mut()?;
    object.insert("modelCatalog".to_string(), model_catalog);
    Some(codex_model_ids_from_settings(&restored_settings))
}

fn sync_codex_desktop_available_models_cache_after_restored_catalog_reload(
    settings: &Value,
    model_catalog: Option<Value>,
) -> bool {
    let model_ids = codex_desktop_available_models_cache_ids_after_restored_catalog_reload(
        settings,
        model_catalog,
    );
    let reloaded = model_ids.is_some();
    sync_codex_desktop_available_models_cache_model_ids(model_ids.unwrap_or_default());
    reloaded
}

/// Reconcile the Desktop whitelist cache after restoring a raw Live backup.
/// Official snapshots without a catalog clear any stale custom future pin.
/// Raw snapshots that point at the cc-switch-owned catalog reload its model IDs
/// and repin them; user-managed external catalog pointers keep their cache
/// free of cc-switch-owned additions without changing their existing pins.
pub fn sync_codex_desktop_available_models_cache_after_live_restore(settings: &Value) {
    if settings.get("modelCatalog").is_some() {
        sync_codex_desktop_available_models_cache_from_settings(settings);
        return;
    }

    let config_text = settings
        .get("config")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let generated_path = get_codex_model_catalog_path();
    if resolve_cc_switch_catalog_path(config_text, &generated_path).is_some() {
        match read_codex_model_catalog_simplified_from_live() {
            Ok(model_catalog) => {
                if !sync_codex_desktop_available_models_cache_after_restored_catalog_reload(
                    settings,
                    model_catalog,
                ) {
                    log::warn!(
                        "Restored a cc-switch Codex model catalog pointer, but its model IDs could not be reloaded for the Desktop cache"
                    );
                }
            }
            Err(err) => {
                invalidate_codex_desktop_available_models_cache_worker();
                log::warn!(
                    "Failed to reload the restored cc-switch Codex model catalog for the Desktop cache: {err}"
                );
            }
        }
        return;
    }

    let has_external_catalog_pointer =
        codex_config_has_external_model_catalog_pointer(config_text, &generated_path);

    if has_external_catalog_pointer {
        remove_codex_desktop_owned_models_preserving_cache_pins();
    } else {
        sync_codex_desktop_available_models_cache_from_settings(settings);
    }
}

fn codex_desktop_available_models_cache_ids_after_provider_write(
    settings: &Value,
    config_text: Option<&str>,
) -> Option<Vec<String>> {
    let generated_path = get_codex_model_catalog_path();
    let preserves_external_catalog = settings.get("modelCatalog").is_none()
        && config_text.is_some_and(|text| {
            codex_config_has_external_model_catalog_pointer(text, &generated_path)
        });
    if preserves_external_catalog {
        None
    } else {
        Some(codex_model_ids_from_settings(settings))
    }
}

pub(crate) fn sync_codex_desktop_available_models_cache_after_provider_write(
    settings: &Value,
    config_text: Option<&str>,
) {
    match codex_desktop_available_models_cache_ids_after_provider_write(settings, config_text) {
        Some(model_ids) => sync_codex_desktop_available_models_cache_model_ids(model_ids),
        None => remove_codex_desktop_owned_models_preserving_cache_pins(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_desktop_statsig_merge_appends_models_without_duplicates() {
        let data = json!({
            "dynamic_configs": {
                CODEX_DESKTOP_STATSIG_MODELS_CONFIG_ID: {
                    "value": {
                        "available_models": ["gpt-5.5", "gpt-5.6-sol"]
                    }
                }
            }
        });
        let mut wrapper = json!({
            "source": "NetworkNotModified",
            "data": data.to_string()
        });
        let models = vec![
            "gpt-5.6-sol".to_string(),
            "gpt-5.6-terra".to_string(),
            "gpt-5.6-luna".to_string(),
        ];

        assert!(merge_codex_desktop_statsig_available_models(
            &mut wrapper,
            &models
        ));
        assert_eq!(
            codex_desktop_statsig_available_model_ids(&wrapper),
            Some(HashSet::from([
                "gpt-5.5".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-5.6-luna".to_string(),
            ]))
        );
        assert!(!merge_codex_desktop_statsig_available_models(
            &mut wrapper,
            &models
        ));

        let next_models = vec!["gpt-5.6-sol".to_string(), "gpt-5.6-orbit".to_string()];
        assert!(merge_codex_desktop_statsig_available_models(
            &mut wrapper,
            &next_models
        ));
        assert_eq!(
            codex_desktop_statsig_available_model_ids(&wrapper),
            Some(HashSet::from([
                "gpt-5.5".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-orbit".to_string(),
            ]))
        );

        assert!(merge_codex_desktop_statsig_available_models(
            &mut wrapper,
            &[]
        ));
        assert_eq!(
            codex_desktop_statsig_available_model_ids(&wrapper),
            Some(HashSet::from([
                "gpt-5.5".to_string(),
                "gpt-5.6-sol".to_string(),
            ]))
        );
    }

    #[test]
    fn codex_desktop_statsig_merge_skips_entries_without_models_config() {
        let data = json!({
            "dynamic_configs": {
                "unrelated-config": {
                    "value": { "enabled": true }
                }
            }
        });
        let mut wrapper = json!({
            "source": "NetworkNotModified",
            "data": data.to_string()
        });
        let original = wrapper.clone();

        assert!(!merge_codex_desktop_statsig_available_models(
            &mut wrapper,
            &["gpt-5.6-sol".to_string()]
        ));
        assert_eq!(wrapper, original);
    }

    #[test]
    fn codex_desktop_statsig_pin_rejects_sibling_evaluation_collections() {
        let data = json!({
            "dynamic_configs": {
                CODEX_DESKTOP_STATSIG_MODELS_CONFIG_ID: {
                    "value": { "available_models": ["gpt-5.5"] }
                }
            },
            "feature_gates": { "unrelated-gate": { "value": true } },
            "layer_configs": { "unrelated-layer": { "value": { "enabled": true } } }
        });
        let wrapper = json!({
            "source": "Network",
            "data": data.to_string()
        });

        assert!(
            !codex_desktop_statsig_has_only_models_config(&wrapper),
            "evaluation bundles with sibling collections must not be future-pinned"
        );
    }

    #[test]
    fn codex_desktop_statsig_wrapper_round_trips_utf16le_values() {
        let wrapper = json!({
            "source": "NetworkNotModified",
            "data": "{}"
        });
        let text = serde_json::to_string(&wrapper).unwrap();
        let mut encoded = vec![1];
        for unit in text.encode_utf16() {
            encoded.extend_from_slice(&unit.to_le_bytes());
        }

        let (prefix, encoding, decoded) = decode_codex_desktop_statsig_wrapper(&encoded).unwrap();
        assert_eq!(prefix, Some(1));
        assert_eq!(encoding, CodexDesktopStatsigWrapperEncoding::Utf16Le);
        assert_eq!(decoded, wrapper);
        assert_eq!(
            encode_codex_desktop_statsig_wrapper(prefix, encoding, &decoded).unwrap(),
            encoded
        );
    }

    #[test]
    fn codex_desktop_statsig_pin_keeps_custom_cache_newer_than_network_refresh() {
        let mut last_modified = json!({
            "statsig.cached.evaluations.custom": 1_000,
            "statsig.cached.evaluations.network": 2_000,
        });
        let cache_keys = HashSet::from(["statsig.cached.evaluations.custom".to_string()]);

        assert!(pin_codex_desktop_statsig_last_modified_cache_keys(
            &mut last_modified,
            &cache_keys,
            10_000,
        ));
        assert!(
            last_modified["statsig.cached.evaluations.custom"]
                .as_i64()
                .unwrap()
                > last_modified["statsig.cached.evaluations.network"]
                    .as_i64()
                    .unwrap()
        );
    }

    #[test]
    fn codex_desktop_statsig_leveldb_sync_updates_cached_models() {
        let temp_dir = tempfile::tempdir().expect("create temp leveldb");
        let options = rusty_leveldb::Options {
            create_if_missing: true,
            ..Default::default()
        };
        let mut db = rusty_leveldb::DB::open(temp_dir.path(), options).expect("open temp leveldb");
        let key = b"_https://codex\x00statsig.cached.evaluations.active".to_vec();
        let last_modified_key =
            b"_https://codex\x00statsig.last_modified_time.evaluations".to_vec();
        let data = json!({
            "dynamic_configs": {
                CODEX_DESKTOP_STATSIG_MODELS_CONFIG_ID: {
                    "value": { "available_models": ["gpt-5.5"] }
                },
                "unrelated-feature": { "value": { "enabled": true } }
            }
        });
        let wrapper = json!({ "source": "Network", "data": data.to_string() });
        let value = encode_codex_desktop_statsig_wrapper(
            Some(1),
            CodexDesktopStatsigWrapperEncoding::Utf8,
            &wrapper,
        )
        .unwrap();
        db.put(&key, &value).expect("seed cache");
        let last_modified_value = encode_codex_desktop_statsig_wrapper(
            Some(1),
            CodexDesktopStatsigWrapperEncoding::Utf8,
            &json!({ "statsig.cached.evaluations.active": 1_000 }),
        )
        .unwrap();
        db.put(&last_modified_key, &last_modified_value)
            .expect("seed last modified cache");
        db.close().expect("close seeded leveldb");

        let result = sync_codex_desktop_available_models_cache_path_with_status(
            temp_dir.path(),
            &["gpt-5.6-sol".to_string(), "gpt-5.6-terra".to_string()],
        )
        .expect("sync temp leveldb");
        assert_eq!(result.updated_count, 1);
        assert!(
            !result.ready_model_cache,
            "a shared dynamic-config bundle must not be treated as safely pinned"
        );
        assert!(
            result.requires_short_retry,
            "a shared dynamic-config bundle needs short maintenance retries"
        );

        let options = rusty_leveldb::Options {
            create_if_missing: false,
            ..Default::default()
        };
        let mut db = rusty_leveldb::DB::open(temp_dir.path(), options).expect("reopen leveldb");
        let value = db.get(&key).expect("read updated cache");
        let (_, _, wrapper) = decode_codex_desktop_statsig_wrapper(&value).unwrap();
        assert_eq!(
            codex_desktop_statsig_available_model_ids(&wrapper),
            Some(HashSet::from([
                "gpt-5.5".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
            ]))
        );
        let data = serde_json::from_str::<Value>(wrapper["data"].as_str().unwrap()).unwrap();
        assert_eq!(
            data["dynamic_configs"]["unrelated-feature"]["value"]["enabled"],
            true
        );

        let last_modified_value = db
            .get(&last_modified_key)
            .expect("read updated last modified cache");
        let (_, _, last_modified) =
            decode_codex_desktop_statsig_wrapper(&last_modified_value).unwrap();
        assert_eq!(
            last_modified["statsig.cached.evaluations.active"].as_i64(),
            Some(1_000)
        );
        db.close().expect("close leveldb");
    }
}
