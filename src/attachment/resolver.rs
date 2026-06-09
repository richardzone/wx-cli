//! 把 `AttachmentId` 翻译成本地 `.dat` 路径。
//!
//! 流程：
//!   1. `chat` username → `ChatName2Id.rowid`（资源库）
//!   2. `(chat_id, local_id)` + `ORDER BY message_create_time DESC LIMIT 1` →
//!      `MessageResourceInfo.packed_info`
//!   3. 从 `packed_info` (protobuf) 提取 32 字节 ASCII hex MD5
//!   4. 在 `<wxchat_base>/msg/attach/<md5(chat)>/<YYYY-MM>/Img/<md5>[_t|_h].dat`
//!      下找对应文件，按 full > _h > _t 优先级选一个
//!
//! `<wxchat_base>` 由 daemon 已知（同 `db_dir` 的父目录），路径 layout 平台差异：
//! - Linux: `~/Documents/xwechat_files/<wxid>`
//! - macOS: `~/Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files/<wxid>`
//!   ⚠️  msg/attach/... 子树 layout 待我用真实账号验证；上游 docstring 只写了 Windows
//! - Windows: `<root>\xwechat_files\<wxid>`（root 从 `%APPDATA%\Tencent\xwechat\config\*.ini` 读）

use anyhow::{anyhow, Context, Result};
use chrono::TimeZone;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{AttachmentId, AttachmentKind};

/// 单条 attachment 在资源库 + 本地 attach 树下的解析结果。
#[derive(Debug, Clone)]
pub struct ResolvedAttachment {
    pub id: AttachmentId,
    /// 从 `packed_info` 提取出的资源 MD5（小写 hex）
    pub md5: String,
    /// 命中的本地 .dat 路径（按 full > _h > _t 优先级选一个）
    pub dat_path: PathBuf,
    /// 文件 size（字节）
    pub size: u64,
}

/// 仅 schema lookup（不去找本地 .dat）。
/// 用于 `wx attachments` 列表时填 `md5` 字段——文件可能根本不在本地。
#[derive(Debug, Clone)]
pub struct AttachmentMetadata {
    pub md5: String,
}

/// `message/media_0.db::VoiceInfo` 中的一条语音资源。
#[derive(Debug, Clone)]
pub struct ResolvedVoiceMedia {
    pub data: Vec<u8>,
    pub chunks: usize,
    pub svr_id: Option<i64>,
}

/// 用 `(chat, local_id)` 查 message_resource.db 拿 file md5。
///
/// 调用方传已经解密好的 `message_resource.db` 路径（由 daemon 的 `DBCache` 准备）。
/// 同步函数 — caller 在 `spawn_blocking` 里跑。
pub fn lookup_md5_blocking(
    resource_db_path: &Path,
    chat: &str,
    local_id: i64,
    create_time: i64,
    msg_local_type_lo32: i64,
) -> Result<Option<AttachmentMetadata>> {
    let conn = Connection::open_with_flags(
        resource_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("打开 message_resource.db {:?}", resource_db_path))?;

    // 1) ChatName2Id: user_name -> rowid
    let chat_id: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM ChatName2Id WHERE user_name = ?1",
            [chat],
            |row| row.get(0),
        )
        .ok();
    let Some(chat_id) = chat_id else {
        return Ok(None);
    };

    // 2) MessageResourceInfo:
    //    同 chat 内 local_id 会复用，所以先用 create_time 精确命中；
    //    若资源库里的时间戳跟 message_N.db 不完全对齐，再 fallback 到“同 local_id/type 取最新”
    //    message_local_type 高 32 bit 是版本/会话 flag，低 32 bit 才是真实类型
    let packed_exact: Option<Vec<u8>> = conn
        .query_row(
            "SELECT packed_info FROM MessageResourceInfo
             WHERE chat_id = ?1
               AND message_local_id = ?2
               AND (message_local_type = ?3 OR message_local_type % 4294967296 = ?3)
               AND message_create_time = ?4
             ORDER BY rowid DESC
             LIMIT 1",
            rusqlite::params![chat_id, local_id, msg_local_type_lo32, create_time],
            |row| row.get(0),
        )
        .ok();

    let packed: Option<Vec<u8>> = packed_exact.or_else(|| {
        conn.query_row(
            "SELECT packed_info FROM MessageResourceInfo
             WHERE chat_id = ?1
               AND message_local_id = ?2
               AND (message_local_type = ?3 OR message_local_type % 4294967296 = ?3)
             ORDER BY message_create_time DESC
             LIMIT 1",
            rusqlite::params![chat_id, local_id, msg_local_type_lo32],
            |row| row.get(0),
        )
        .ok()
    });

    let Some(blob) = packed else {
        return Ok(None);
    };
    Ok(extract_md5_from_packed_info(&blob).map(|md5| AttachmentMetadata { md5 }))
}

/// 从 `message/media_0.db` 的 VoiceInfo 表读取语音 BLOB。
///
/// WeChat 4.x 语音不一定进入 `message_resource.db`，常见路径是：
/// `media_0.db::VoiceInfo(local_id, create_time, voice_data, data_index)`。
/// `data_index` 预留分片能力，所以这里按 data_index 顺序拼接同一条语音的所有 chunk。
pub fn lookup_voice_media_blocking(
    media_db_path: &Path,
    chat: &str,
    local_id: i64,
    create_time: i64,
) -> Result<Option<ResolvedVoiceMedia>> {
    let conn = Connection::open_with_flags(
        media_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("打开 media_0.db {:?}", media_db_path))?;

    let has_voice_info: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='VoiceInfo'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has_voice_info {
        return Ok(None);
    }

    let columns = table_columns(&conn, "VoiceInfo")?;
    if !columns.contains("voice_data") {
        return Ok(None);
    }
    let data_index_expr = if columns.contains("data_index") {
        "CAST(COALESCE(data_index, '0') AS INTEGER)"
    } else {
        "0"
    };
    let svr_id_expr = if columns.contains("svr_id") {
        "svr_id"
    } else {
        "NULL"
    };

    let mut rows = Vec::new();

    if columns.contains("local_id") {
        if columns.contains("chat_name_id") {
            let chat_id: Option<i64> = conn
                .query_row(
                    "SELECT rowid FROM Name2Id WHERE user_name = ?1",
                    [chat],
                    |row| row.get(0),
                )
                .ok();

            let Some(chat_id) = chat_id else {
                return Ok(None);
            };

            if columns.contains("create_time") {
                rows = query_voice_rows(
                    &conn,
                    "chat_name_id = ?1 AND local_id = ?2 AND create_time = ?3",
                    rusqlite::params![chat_id, local_id, create_time],
                    data_index_expr,
                    svr_id_expr,
                )?;
            }
            if rows.is_empty() && !columns.contains("create_time") {
                rows = query_voice_rows(
                    &conn,
                    "chat_name_id = ?1 AND local_id = ?2",
                    rusqlite::params![chat_id, local_id],
                    data_index_expr,
                    svr_id_expr,
                )?;
            }
        }
    }

    if rows.is_empty() && columns.contains("msgid") {
        if !columns.contains("user_name") {
            return Ok(None);
        }
        if columns.contains("msgtime") {
            rows = query_voice_rows(
                &conn,
                "user_name = ?1 AND msgid = ?2 AND msgtime = ?3",
                rusqlite::params![chat, local_id, create_time],
                data_index_expr,
                svr_id_expr,
            )?;
        }
        if rows.is_empty() && !columns.contains("msgtime") {
            rows = query_voice_rows(
                &conn,
                "user_name = ?1 AND msgid = ?2",
                rusqlite::params![chat, local_id],
                data_index_expr,
                svr_id_expr,
            )?;
        }
    }

    if rows.is_empty() {
        return Ok(None);
    }

    rows.sort_by_key(|row| row.0);
    let svr_id = rows.iter().find_map(|row| row.2);
    let chunks = rows.len();
    let total_len: usize = rows.iter().map(|row| row.1.len()).sum();
    if total_len == 0 {
        return Ok(None);
    }
    let mut data = Vec::with_capacity(total_len);
    for (_idx, chunk, _svr_id) in rows {
        data.extend_from_slice(&chunk);
    }

    Ok(Some(ResolvedVoiceMedia {
        data,
        chunks,
        svr_id,
    }))
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(columns)
}

fn query_voice_rows<P>(
    conn: &Connection,
    where_clause: &str,
    params: P,
    data_index_expr: &str,
    svr_id_expr: &str,
) -> Result<Vec<(i64, Vec<u8>, Option<i64>)>>
where
    P: rusqlite::Params,
{
    let sql = format!(
        "SELECT {data_index_expr} AS voice_index, voice_data, {svr_id_expr} AS voice_svr_id
         FROM VoiceInfo
         WHERE {where_clause}
         ORDER BY voice_index, rowid"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params, |row| {
            Ok((
                row.get::<_, i64>(0).unwrap_or(0),
                row.get::<_, Vec<u8>>(1).unwrap_or_default(),
                row.get::<_, i64>(2).ok(),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 从 `MessageResourceInfo.packed_info` (protobuf) 提取 32 字节 ASCII hex md5。
///
/// 主路径：搜 4 字节 marker `12 22 0a 20`（field=2 LEN, length=34, sub field=1 LEN, length=32），
/// 紧跟 32 字节 ASCII hex。
/// Fallback：扫整个 blob 找连续 32 字节合法 hex 字符。
pub fn extract_md5_from_packed_info(blob: &[u8]) -> Option<String> {
    const MARKER: &[u8; 4] = &[0x12, 0x22, 0x0A, 0x20];

    // 主路径
    if let Some(pos) = find_subslice(blob, MARKER) {
        let start = pos + MARKER.len();
        if start + 32 <= blob.len() {
            if let Ok(s) = std::str::from_utf8(&blob[start..start + 32]) {
                if s.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(s.to_ascii_lowercase());
                }
            }
        }
    }

    // Fallback：连续 32 字节合法 hex
    if blob.len() >= 32 {
        for start in 0..=blob.len() - 32 {
            let chunk = &blob[start..start + 32];
            if let Ok(s) = std::str::from_utf8(chunk) {
                if s.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(s.to_ascii_lowercase());
                }
            }
        }
    }
    None
}

/// 简单的子串扫描（避免拉 memchr/memmem 依赖；blob 通常 < 1KB）
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 在 `<attach_root>/<md5(chat)>/<YYYY-MM>/Img/<md5>[_t|_h].dat` 下找图片文件。
///
/// 优先级：full > `_h`（HD thumbnail）> `_t`（thumbnail）。返回最优的一个；
/// 找不到返回 None。
///
/// `attach_root` = `<wxchat_base>/msg/attach`。
/// `create_time` 用于先定位 `<YYYY-MM>` 子目录；找不到时再 fallback 全月份扫描，
/// 因为 WeChat 的 `YYYY-MM` 目录有时跟消息时间差 1 个月（按收到时间归档）。
pub fn find_dat_file(
    attach_root: &Path,
    chat: &str,
    file_md5: &str,
    create_time: i64,
) -> Option<PathBuf> {
    find_media_file(
        attach_root,
        chat,
        file_md5,
        create_time,
        AttachmentKind::Image,
    )
}

/// 在本地附件树中定位指定 kind 的媒体文件。
///
/// image 走已经验证过的 `Img/<md5>[_h|_t].dat` 规则；voice 是 POC 路径，优先试
/// `Voice` / `Audio` 目录里的 md5 同名文件，最后在 `msg/attach` 下按 md5 前缀递归兜底。
pub fn find_media_file(
    attach_root: &Path,
    chat: &str,
    file_md5: &str,
    create_time: i64,
    kind: AttachmentKind,
) -> Option<PathBuf> {
    let chat_hash = format!("{:x}", md5::compute(chat.as_bytes()));
    let chat_dir = attach_root.join(&chat_hash);
    if !chat_dir.is_dir() {
        return match kind {
            AttachmentKind::Voice => find_by_md5_recursive(attach_root, file_md5, kind),
            _ => None,
        };
    }

    // 第一步：试 create_time 当月 + 前后各一个月（共 3 个候选目录）
    let candidates_ym: Vec<String> = three_month_candidates(create_time);
    for ym in &candidates_ym {
        if let Some(p) = pick_best_in_month_dir(&chat_dir.join(ym), file_md5, kind) {
            return Some(p);
        }
    }

    // 第二步 fallback：扫整个 chat_dir 的所有月份子目录
    let entries = std::fs::read_dir(&chat_dir).ok()?;
    let mut all_months: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // 已经试过的 3 个候选可以跳过，但成本极小；保留全量扫
    all_months.sort();
    for month_dir in all_months {
        if let Some(p) = pick_best_in_month_dir(&month_dir, file_md5, kind) {
            return Some(p);
        }
    }

    // POC fallback：Mac 4.x 的语音路径未完全验证。若上面的目录名猜错，仍按资源 md5
    // 在 attach 树下递归找一次，避免因为 `Voice`/`Audio` 布局差异直接失败。
    match kind {
        AttachmentKind::Voice => find_by_md5_recursive(attach_root, file_md5, kind),
        _ => None,
    }
}

fn pick_best_in_month_dir(
    month_dir: &Path,
    file_md5: &str,
    kind: AttachmentKind,
) -> Option<PathBuf> {
    match kind {
        AttachmentKind::Image => pick_best_in_img_dir(&month_dir.join("Img"), file_md5),
        AttachmentKind::Voice => {
            for subdir in ["Voice", "Audio", "Aud"] {
                if let Some(p) = pick_best_media_file(&month_dir.join(subdir), file_md5, kind) {
                    return Some(p);
                }
            }
            None
        }
        AttachmentKind::Video => pick_best_media_file(&month_dir.join("Video"), file_md5, kind),
        AttachmentKind::File => pick_best_media_file(month_dir, file_md5, kind),
    }
}

fn pick_best_in_img_dir(img_dir: &Path, file_md5: &str) -> Option<PathBuf> {
    if !img_dir.is_dir() {
        return None;
    }
    let full = img_dir.join(format!("{}.dat", file_md5));
    if full.is_file() {
        return Some(full);
    }
    let hd = img_dir.join(format!("{}_h.dat", file_md5));
    if hd.is_file() {
        return Some(hd);
    }
    let thumb = img_dir.join(format!("{}_t.dat", file_md5));
    if thumb.is_file() {
        return Some(thumb);
    }
    None
}

fn pick_best_media_file(media_dir: &Path, file_md5: &str, kind: AttachmentKind) -> Option<PathBuf> {
    if !media_dir.is_dir() {
        return None;
    }

    for name in exact_media_names(file_md5, kind) {
        let path = media_dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut candidates = media_dir
        .read_dir()
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|name| name.starts_with(file_md5))
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|p| {
        let size = p.metadata().map(|m| m.len()).unwrap_or(0);
        std::cmp::Reverse(size)
    });
    candidates.into_iter().next()
}

fn exact_media_names(file_md5: &str, kind: AttachmentKind) -> Vec<String> {
    match kind {
        AttachmentKind::Image => vec![
            format!("{}.dat", file_md5),
            format!("{}_h.dat", file_md5),
            format!("{}_t.dat", file_md5),
        ],
        AttachmentKind::Voice => ["", ".aud", ".amr", ".silk", ".wav", ".m4a", ".mp3", ".dat"]
            .iter()
            .map(|ext| format!("{}{}", file_md5, ext))
            .collect(),
        AttachmentKind::Video => [".mp4", ".mov", ".m4v", ".dat"]
            .iter()
            .map(|ext| format!("{}{}", file_md5, ext))
            .collect(),
        AttachmentKind::File => vec![file_md5.to_string()],
    }
}

fn find_by_md5_recursive(root: &Path, file_md5: &str, kind: AttachmentKind) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    let mut stack = vec![root.to_path_buf()];
    let mut matches = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name == file_md5
                || exact_media_names(file_md5, kind).iter().any(|n| n == name)
                || name.starts_with(file_md5)
            {
                matches.push(path);
            }
        }
    }
    matches.sort_by_key(|p| {
        let size = p.metadata().map(|m| m.len()).unwrap_or(0);
        std::cmp::Reverse(size)
    });
    matches.into_iter().next()
}

fn three_month_candidates(unix_ts: i64) -> Vec<String> {
    use chrono::{Datelike, Duration};
    let dt = match chrono::Local.timestamp_opt(unix_ts, 0).single() {
        Some(d) => d,
        None => return Vec::new(),
    };
    let prev = dt - Duration::days(31);
    let next = dt + Duration::days(31);
    [prev, dt, next]
        .iter()
        .map(|d| format!("{:04}-{:02}", d.year(), d.month()))
        .collect()
}

/// 把 `<wxchat_base>` （即 `db_storage` 父目录）拼成 `<base>/msg/attach`。
pub fn attach_root_for(wxchat_base: &Path) -> PathBuf {
    wxchat_base.join("msg").join("attach")
}

/// 完整流程：用 `attachment_id` 拿 md5 + 找 .dat。失败返回带具体诊断信息的 `Err`。
///
/// `resource_db_path` 由 daemon 提供（DBCache 已经解密好）；
/// `attach_root` 由 caller 拼好（`attach_root_for(wxchat_base)`）。
/// 同步函数 — caller 在 `spawn_blocking` 里跑。
pub fn resolve_blocking(
    id: &AttachmentId,
    resource_db_path: &Path,
    attach_root: &Path,
) -> Result<ResolvedAttachment> {
    let lo32_type: i64 = match id.kind {
        super::AttachmentKind::Image => 3,
        super::AttachmentKind::Voice => 34,
        super::AttachmentKind::Video => 43,
        super::AttachmentKind::File => 49,
    };

    let meta = lookup_md5_blocking(
        resource_db_path,
        &id.chat,
        id.local_id,
        id.create_time,
        lo32_type,
    )?
        .ok_or_else(|| {
            anyhow!(
                "message_resource.db 中找不到 chat={} local_id={} type={} 的资源行（可能是非附件消息或资源库未同步）",
                id.chat,
                id.local_id,
                lo32_type
            )
        })?;

    let dat_path =
        find_media_file(attach_root, &id.chat, &meta.md5, id.create_time, id.kind).ok_or_else(
            || {
                anyhow!(
                    "找不到本地附件文件（kind={} md5={} chat={} create_time={}）— 微信可能尚未下载该附件，或附件已被清理",
                    id.kind.as_str(),
                    meta.md5,
                    id.chat,
                    id.create_time
                )
            },
        )?;
    let size = std::fs::metadata(&dat_path).map(|m| m.len()).unwrap_or(0);

    Ok(ResolvedAttachment {
        id: id.clone(),
        md5: meta.md5,
        dat_path,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_md5_main_path() {
        // 构造一段含 12 22 0a 20 marker 的 blob
        let mut blob = vec![0xAA, 0xBB, 0xCC];
        blob.extend_from_slice(&[0x12, 0x22, 0x0A, 0x20]);
        blob.extend_from_slice(b"deadbeefcafebabe1234567890abcdef");
        blob.extend_from_slice(&[0xFF, 0xFF]);
        assert_eq!(
            extract_md5_from_packed_info(&blob),
            Some("deadbeefcafebabe1234567890abcdef".to_string())
        );
    }

    #[test]
    fn extract_md5_fallback_no_marker() {
        // 没有 marker，但 blob 里有合法 32 字节 hex
        let mut blob = vec![0xFF, 0x00];
        blob.extend_from_slice(b"00112233445566778899aabbccddeeff");
        blob.extend_from_slice(&[0x01]);
        assert_eq!(
            extract_md5_from_packed_info(&blob),
            Some("00112233445566778899aabbccddeeff".to_string())
        );
    }

    #[test]
    fn extract_md5_uppercase_normalized_to_lower() {
        let mut blob = vec![0x12, 0x22, 0x0A, 0x20];
        blob.extend_from_slice(b"DEADBEEFCAFEBABE1234567890ABCDEF");
        // 上游/CI/本地 file md5 都是 lowercase；强制小写化避免大小写不一致导致命中失败
        assert_eq!(
            extract_md5_from_packed_info(&blob),
            Some("deadbeefcafebabe1234567890abcdef".to_string())
        );
    }

    #[test]
    fn extract_md5_returns_none_on_garbage() {
        let blob = vec![0; 16];
        assert!(extract_md5_from_packed_info(&blob).is_none());
    }

    #[test]
    fn lookup_md5_prefers_exact_create_time_over_latest_reuse() {
        let dir = tempdir_for_test();
        let db_path = dir.join("message_resource.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE ChatName2Id (user_name TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO ChatName2Id (rowid, user_name) VALUES (1, 'room@chatroom')",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE MessageResourceInfo (
                chat_id INTEGER,
                message_local_id INTEGER,
                message_local_type INTEGER,
                message_create_time INTEGER,
                packed_info BLOB
            )",
            [],
        )
        .unwrap();

        let old_blob = {
            let mut blob = vec![0x12, 0x22, 0x0A, 0x20];
            blob.extend_from_slice(b"11111111111111111111111111111111");
            blob
        };
        let new_blob = {
            let mut blob = vec![0x12, 0x22, 0x0A, 0x20];
            blob.extend_from_slice(b"22222222222222222222222222222222");
            blob
        };

        conn.execute(
            "INSERT INTO MessageResourceInfo
             (chat_id, message_local_id, message_local_type, message_create_time, packed_info)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![1i64, 7i64, 3i64, 1000i64, old_blob],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO MessageResourceInfo
             (chat_id, message_local_id, message_local_type, message_create_time, packed_info)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![1i64, 7i64, 3i64, 2000i64, new_blob],
        )
        .unwrap();

        let old = lookup_md5_blocking(&db_path, "room@chatroom", 7, 1000, 3)
            .unwrap()
            .unwrap();
        let new = lookup_md5_blocking(&db_path, "room@chatroom", 7, 2000, 3)
            .unwrap()
            .unwrap();
        assert_eq!(old.md5, "11111111111111111111111111111111");
        assert_eq!(new.md5, "22222222222222222222222222222222");
    }

    #[test]
    fn lookup_voice_media_reads_chunks_from_media_db() {
        let dir = tempdir_for_test();
        let db_path = dir.join("media_0.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO Name2Id (rowid, user_name) VALUES (9, 'room@chatroom')",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE VoiceInfo (
                chat_name_id INTEGER,
                create_time INTEGER,
                local_id INTEGER,
                svr_id INTEGER,
                voice_data BLOB,
                data_index TEXT DEFAULT '0'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO VoiceInfo
             (chat_name_id, create_time, local_id, svr_id, voice_data, data_index)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![9i64, 2000i64, 7i64, 123i64, b"two", "2"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO VoiceInfo
             (chat_name_id, create_time, local_id, svr_id, voice_data, data_index)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![9i64, 2000i64, 7i64, 123i64, b"one", "1"],
        )
        .unwrap();

        let media = lookup_voice_media_blocking(&db_path, "room@chatroom", 7, 2000)
            .unwrap()
            .unwrap();
        assert_eq!(media.data, b"onetwo");
        assert_eq!(media.chunks, 2);
        assert_eq!(media.svr_id, Some(123));
    }

    #[test]
    fn lookup_voice_media_keeps_rows_scoped_to_chat() {
        let dir = tempdir_for_test();
        let db_path = dir.join("media_0.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO Name2Id (rowid, user_name) VALUES (9, 'room@chatroom')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Name2Id (rowid, user_name) VALUES (10, 'other@chatroom')",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE VoiceInfo (
                chat_name_id INTEGER,
                create_time INTEGER,
                local_id INTEGER,
                svr_id INTEGER,
                voice_data BLOB,
                data_index TEXT DEFAULT '0'
            )",
            [],
        )
        .unwrap();
        for (chat_id, data) in [(10i64, b"wrong".as_slice()), (9i64, b"right".as_slice())] {
            conn.execute(
                "INSERT INTO VoiceInfo
                 (chat_name_id, create_time, local_id, svr_id, voice_data, data_index)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![chat_id, 2000i64, 7i64, 123i64, data, "0"],
            )
            .unwrap();
        }

        let media = lookup_voice_media_blocking(&db_path, "room@chatroom", 7, 2000)
            .unwrap()
            .unwrap();
        assert_eq!(media.data, b"right");
    }

    #[test]
    fn lookup_voice_media_uses_create_time_to_disambiguate_reused_local_id() {
        let dir = tempdir_for_test();
        let db_path = dir.join("media_0.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE Name2Id (user_name TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO Name2Id (rowid, user_name) VALUES (9, 'room@chatroom')",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE VoiceInfo (
                chat_name_id INTEGER,
                create_time INTEGER,
                local_id INTEGER,
                svr_id INTEGER,
                voice_data BLOB,
                data_index TEXT DEFAULT '0'
            )",
            [],
        )
        .unwrap();
        for (create_time, data) in [(1000i64, b"old".as_slice()), (2000i64, b"new".as_slice())] {
            conn.execute(
                "INSERT INTO VoiceInfo
                 (chat_name_id, create_time, local_id, svr_id, voice_data, data_index)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![9i64, create_time, 7i64, 123i64, data, "0"],
            )
            .unwrap();
        }

        let media = lookup_voice_media_blocking(&db_path, "room@chatroom", 7, 2000)
            .unwrap()
            .unwrap();
        assert_eq!(media.data, b"new");
        assert!(
            lookup_voice_media_blocking(&db_path, "room@chatroom", 7, 3000)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lookup_voice_media_reads_legacy_schema_without_chunk_columns() {
        let dir = tempdir_for_test();
        let db_path = dir.join("media_0.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE VoiceInfo (
                user_name TEXT,
                msgid INTEGER,
                msgtime INTEGER,
                voice_data BLOB
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO VoiceInfo (user_name, msgid, msgtime, voice_data)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["room@chatroom", 7i64, 2000i64, b"voice"],
        )
        .unwrap();

        let media = lookup_voice_media_blocking(&db_path, "room@chatroom", 7, 2000)
            .unwrap()
            .unwrap();
        assert_eq!(media.data, b"voice");
        assert_eq!(media.chunks, 1);
        assert_eq!(media.svr_id, None);
    }

    #[test]
    fn lookup_voice_media_legacy_schema_uses_msgtime_to_disambiguate_reused_msgid() {
        let dir = tempdir_for_test();
        let db_path = dir.join("media_0.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE VoiceInfo (
                user_name TEXT,
                msgid INTEGER,
                msgtime INTEGER,
                voice_data BLOB
            )",
            [],
        )
        .unwrap();
        for (msgtime, data) in [(1000i64, b"old".as_slice()), (2000i64, b"new".as_slice())] {
            conn.execute(
                "INSERT INTO VoiceInfo (user_name, msgid, msgtime, voice_data)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["room@chatroom", 7i64, msgtime, data],
            )
            .unwrap();
        }

        let media = lookup_voice_media_blocking(&db_path, "room@chatroom", 7, 2000)
            .unwrap()
            .unwrap();
        assert_eq!(media.data, b"new");
        assert!(
            lookup_voice_media_blocking(&db_path, "room@chatroom", 7, 3000)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn three_month_candidates_includes_prev_curr_next() {
        // 2025-08-15 (mid-month) → 2025-07, 2025-08, 2025-09
        let ts = chrono::Local
            .with_ymd_and_hms(2025, 8, 15, 12, 0, 0)
            .unwrap()
            .timestamp();
        let v = three_month_candidates(ts);
        assert!(v.contains(&"2025-07".to_string()));
        assert!(v.contains(&"2025-08".to_string()));
        assert!(v.contains(&"2025-09".to_string()));
    }

    #[test]
    fn pick_best_prefers_full_then_h_then_t() {
        let tmp = tempdir_for_test();
        let img = tmp.join("Img");
        std::fs::create_dir_all(&img).unwrap();
        let md5 = "abcd1234";
        std::fs::write(img.join(format!("{}_t.dat", md5)), b"thumb").unwrap();
        std::fs::write(img.join(format!("{}_h.dat", md5)), b"hd").unwrap();
        // 只有 _t / _h 时取 _h
        assert_eq!(
            pick_best_in_img_dir(&img, md5)
                .unwrap()
                .file_name()
                .unwrap(),
            format!("{}_h.dat", md5).as_str()
        );
        // 加 full 后取 full
        std::fs::write(img.join(format!("{}.dat", md5)), b"full").unwrap();
        assert_eq!(
            pick_best_in_img_dir(&img, md5)
                .unwrap()
                .file_name()
                .unwrap(),
            format!("{}.dat", md5).as_str()
        );
    }

    #[test]
    fn find_media_file_finds_voice_by_month_voice_dir() {
        let tmp = tempdir_for_test();
        let chat = "room@chatroom";
        let chat_hash = format!("{:x}", md5::compute(chat.as_bytes()));
        let ts = chrono::Local
            .with_ymd_and_hms(2026, 6, 9, 12, 0, 0)
            .unwrap()
            .timestamp();
        let voice_dir = tmp.join(chat_hash).join("2026-06").join("Voice");
        std::fs::create_dir_all(&voice_dir).unwrap();
        let md5 = "00112233445566778899aabbccddeeff";
        std::fs::write(voice_dir.join(format!("{}.aud", md5)), b"voice").unwrap();

        let found = find_media_file(&tmp, chat, md5, ts, AttachmentKind::Voice).unwrap();
        assert_eq!(found.file_name().unwrap(), format!("{}.aud", md5).as_str());
    }

    #[test]
    fn find_media_file_voice_recurses_when_layout_unknown() {
        let tmp = tempdir_for_test();
        let chat = "room@chatroom";
        let ts = chrono::Local
            .with_ymd_and_hms(2026, 6, 9, 12, 0, 0)
            .unwrap()
            .timestamp();
        let odd_dir = tmp.join("somehash").join("2026-06").join("NotVoice");
        std::fs::create_dir_all(&odd_dir).unwrap();
        let md5 = "abcdefabcdefabcdefabcdefabcdefab";
        std::fs::write(odd_dir.join(format!("{}.silk", md5)), b"voice").unwrap();

        let found = find_media_file(&tmp, chat, md5, ts, AttachmentKind::Voice).unwrap();
        assert_eq!(found.file_name().unwrap(), format!("{}.silk", md5).as_str());
    }

    fn tempdir_for_test() -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("wx-cli-attach-test-{}-{}", pid, nanos));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
