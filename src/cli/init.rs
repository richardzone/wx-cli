use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config;
use crate::scanner::{self, ScanOptions};

pub fn cmd_init(
    force: bool,
    db_dir_arg: Option<PathBuf>,
    app_arg: Option<PathBuf>,
    bundle_id_arg: Option<String>,
    wechat_process_arg: Option<String>,
) -> Result<()> {
    // 查找 config.json
    let config_path = find_or_create_config_path();
    let existing_cfg = read_config_map(&config_path);

    // 检查是否已初始化
    let selector_supplied = db_dir_arg.is_some()
        || app_arg.is_some()
        || bundle_id_arg.is_some()
        || wechat_process_arg.is_some();
    let target_selector_supplied =
        db_dir_arg.is_some() || app_arg.is_some() || bundle_id_arg.is_some();
    if !force && !selector_supplied {
        if let Some(cfg) = existing_cfg.as_ref() {
            let db_dir = cfg.get("db_dir").and_then(|v| v.as_str()).unwrap_or("");
            let keys_file = cfg
                .get("keys_file")
                .and_then(|v| v.as_str())
                .unwrap_or("all_keys.json");
            let keys_path = if std::path::Path::new(keys_file).is_absolute() {
                std::path::PathBuf::from(keys_file)
            } else {
                config_path
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join(keys_file)
            };
            if !db_dir.is_empty()
                && !db_dir.contains("your_wxid")
                && std::path::Path::new(db_dir).exists()
                && keys_path.exists()
            {
                println!("已初始化，数据目录: {}", db_dir);
                println!("如需重新扫描密钥，使用 --force");
                return Ok(());
            }
        }
    }

    let existing_db_dir = if target_selector_supplied {
        None
    } else {
        existing_string(&existing_cfg, "db_dir").map(PathBuf::from)
    };
    let app_path = match app_arg {
        Some(path) => Some(normalize_path(path)),
        None if !target_selector_supplied => existing_string(&existing_cfg, "app_path")
            .map(PathBuf::from)
            .map(normalize_path),
        None => None,
    };
    let bundle_id_arg = match bundle_id_arg {
        Some(id) if !id.trim().is_empty() => Some(id),
        Some(_) => None,
        None if !target_selector_supplied => existing_string(&existing_cfg, "bundle_id"),
        None => None,
    };
    let db_dir_for_bundle = db_dir_arg.as_deref().or(existing_db_dir.as_deref());
    let bundle_id = resolve_bundle_id(bundle_id_arg, app_path.as_deref(), db_dir_for_bundle);
    if db_dir_arg.is_none()
        && existing_db_dir.is_none()
        && app_path.is_some()
        && bundle_id.is_none()
    {
        anyhow::bail!("无法从 --app 读取 bundle id；请同时传 --bundle-id 或 --db-dir");
    }
    let wechat_process = wechat_process_arg
        .or_else(|| existing_string(&existing_cfg, "wechat_process"))
        .unwrap_or_else(default_process_name);

    // Step 1: 检测 db_dir
    let db_dir = if let Some(db_dir) = db_dir_arg {
        let db_dir = normalize_path(db_dir);
        if !db_dir.is_dir() {
            anyhow::bail!("指定的 db_storage 目录不存在: {}", db_dir.display());
        }
        db_dir
    } else if let Some(db_dir) = existing_db_dir.filter(|p| p.is_dir()) {
        normalize_path(db_dir)
    } else {
        println!("检测微信数据目录...");
        config::auto_detect_db_dir_for_bundle(bundle_id.as_deref()).with_context(|| {
            let bundle_hint = bundle_id
                .as_deref()
                .map(|id| format!("（bundle_id: {id}）"))
                .unwrap_or_default();
            format!(
                "未能自动检测到微信数据目录{bundle_hint}\n\
                 请编辑配置文件并填写 db_dir 字段:\n  \
                 {}\n\
                 或运行: wx init --db-dir <data_root>/xwechat_files/<wxid>/db_storage",
                config_path.display()
            )
        })?
    };
    println!("找到数据目录: {}", db_dir.display());
    if let Some(bundle_id) = bundle_id.as_deref() {
        println!("目标 bundle id: {}", bundle_id);
    }
    if let Some(app_path) = app_path.as_deref() {
        println!("目标 App: {}", app_path.display());
    }

    // Step 2: 扫描密钥（需要 root/sudo）
    println!("扫描加密密钥（需要 root 权限）...");
    let scan_opts = ScanOptions {
        process_name: Some(wechat_process.clone()),
        bundle_id: bundle_id.clone(),
        app_path: app_path.clone(),
    };
    let entries = scanner::scan_keys(&db_dir, &scan_opts)?;

    // === 权限边界 ===
    // 扫描完成后立即 drop 到调用用户身份，后续文件写入都是用户属主。
    // 未来 daemon（由 `wx sessions` 以用户身份 fork）才能往 ~/.wx-cli/
    // 写 socket/log/pid。
    #[cfg(unix)]
    drop_privileges_if_sudo()?;

    // 确保父目录存在（如 ~/.wx-cli/），必须在任何写入之前
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }

    // Step 3: 保存 all_keys.json
    let keys_file_path = config_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("all_keys.json");

    let mut keys_json = serde_json::Map::new();
    for entry in &entries {
        keys_json.insert(
            entry.db_name.clone(),
            json!({
                "enc_key": entry.enc_key,
            }),
        );
    }
    std::fs::write(&keys_file_path, serde_json::to_string_pretty(&keys_json)?)
        .context("写入 all_keys.json 失败")?;
    println!("成功提取 {} 个数据库密钥", entries.len());
    println!("密钥已保存: {}", keys_file_path.display());

    // Step 4: 保存 config.json
    let mut cfg = HashMap::new();
    // 读取已有配置
    if config_path.exists() {
        if let Ok(c) = std::fs::read_to_string(&config_path) {
            if let Ok(v) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&c) {
                for (k, val) in v {
                    cfg.insert(k, val);
                }
            }
        }
    }
    cfg.insert("db_dir".into(), json!(db_dir.to_string_lossy()));
    cfg.entry("keys_file".into())
        .or_insert_with(|| json!("all_keys.json"));
    cfg.entry("decrypted_dir".into())
        .or_insert_with(|| json!("decrypted"));
    cfg.insert("wechat_process".into(), json!(wechat_process));
    if let Some(bundle_id) = bundle_id {
        cfg.insert("bundle_id".into(), json!(bundle_id));
    }
    if let Some(app_path) = app_path {
        cfg.insert("app_path".into(), json!(app_path.to_string_lossy()));
    }

    std::fs::write(&config_path, serde_json::to_string_pretty(&cfg)?)
        .context("写入 config.json 失败")?;
    println!("配置已保存: {}", config_path.display());

    // init 之后必须停掉旧 daemon（它用的是旧 config），下次调用会自动重启
    let _ = crate::cli::transport::stop_daemon();

    println!("初始化完成，可以使用 wx sessions / wx history 等命令了");

    #[cfg(target_os = "macos")]
    {
        eprintln!();
        eprintln!("[macOS] 副作用提示：");
        eprintln!("   如果你是通过对 /Applications/WeChat.app 做 ad-hoc 重签来让 init 走通的，");
        eprintln!("   之后 macOS 可能弹 \"微信\" 想访问其他 App 的数据（在微信里打开公众号文章");
        eprintln!("   时尤其常见）。这是 ad-hoc 重签后 WeChat 的 code identity 变了导致的，");
        eprintln!("   不是 wx-cli 在读其他 App 数据。");
        eprintln!("   完整说明：https://github.com/jackwener/wx-cli/blob/main/docs/macos-permission-guide.md#六微信-想访问其他-app-的数据-弹窗");
        eprintln!("   （如果你的 WeChat 仍是 Apple 官方签名、init 是靠 GUI Terminal + 开发者工具");
        eprintln!("    授权走通的，则不会出现这个弹窗，可以忽略本提示。）");
    }

    Ok(())
}

fn read_config_map(
    config_path: &std::path::Path,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let content = std::fs::read_to_string(config_path).ok()?;
    serde_json::from_str::<serde_json::Value>(&content)
        .ok()?
        .as_object()
        .cloned()
}

fn existing_string(
    cfg: &Option<serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    cfg.as_ref()?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn normalize_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn default_process_name() -> String {
    #[cfg(target_os = "macos")]
    {
        "WeChat".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        "wechat".to_string()
    }
    #[cfg(target_os = "windows")]
    {
        "Weixin.exe".to_string()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "WeChat".to_string()
    }
}

fn resolve_bundle_id(
    explicit: Option<String>,
    app_path: Option<&std::path::Path>,
    db_dir: Option<&std::path::Path>,
) -> Option<String> {
    if matches!(explicit.as_deref(), Some(s) if !s.trim().is_empty()) {
        return explicit;
    }
    #[cfg(target_os = "macos")]
    {
        app_path
            .and_then(config::macos_bundle_id_from_app)
            .or_else(|| db_dir.and_then(config::macos_bundle_id_from_db_dir))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app_path;
        let _ = db_dir;
        None
    }
}

/// 如果当前以 root 身份运行且是通过 sudo 启动的，drop 到调用用户身份，
/// 并迁移旧版本遗留的 root 属主 `~/.wx-cli/`。
///
/// 只影响本进程；daemon（后续 fork）会继承调用用户身份。
#[cfg(unix)]
fn drop_privileges_if_sudo() -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    // 当前不是 root（用户直接以非 root 跑的 `wx init`）→ 什么都不做
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }

    let sudo_uid: Option<u32> = std::env::var("SUDO_UID").ok().and_then(|s| s.parse().ok());
    let sudo_gid: Option<u32> = std::env::var("SUDO_GID").ok().and_then(|s| s.parse().ok());
    let (uid, gid) = match (sudo_uid, sudo_gid) {
        (Some(u), Some(g)) if u != 0 => (u, g),
        // 直接以 root 登陆（非 sudo），没有"调用用户"可还原 → 保持 root
        _ => return Ok(()),
    };

    // 迁移旧版本遗留：如果 ~/.wx-cli/ 已存在且属 root，把它 chown 回调用用户，
    // 顺便把 raw key 文件的权限也收紧到 0600（旧版默认 0644，世界可读等于泄露）。
    // 这些必须在 setuid 之前做：chown 需要 root，chmod 也只有属主或 root 能改。
    let cli_base_dir = config::cli_base_dir();
    if cli_base_dir.exists() {
        let _ = chown_recursive(&cli_base_dir, uid, gid);
        let _ = tighten_perms(&cli_base_dir);
    }

    // 设置 umask，让后续 create 出来的文件/目录默认是 0600 / 0700。
    unsafe {
        libc::umask(0o077);
    }

    // 必须先 setgid 再 setuid：一旦 uid 降下来就没法再改 gid 了。
    unsafe {
        if libc::setgid(gid) != 0 {
            anyhow::bail!("setgid({}) 失败: {}", gid, std::io::Error::last_os_error());
        }
        if libc::setuid(uid) != 0 {
            anyhow::bail!("setuid({}) 失败: {}", uid, std::io::Error::last_os_error());
        }
    }

    // chown 递归实现
    fn chown_recursive(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
        chown_one(path, uid, gid)?;
        let md = std::fs::symlink_metadata(path)?;
        if md.is_dir() {
            for entry in std::fs::read_dir(path)? {
                chown_recursive(&entry?.path(), uid, gid)?;
            }
        }
        Ok(())
    }
    fn chown_one(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL")
        })?;
        if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// 目录收紧到 0700，所有 *.json 文件（含 all_keys.json 这类 raw key）收紧到 0600。
    fn tighten_perms(cli_dir: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cli_dir, std::fs::Permissions::from_mode(0o700))?;
        for entry in std::fs::read_dir(cli_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        Ok(())
    }

    Ok(())
}

fn find_or_create_config_path() -> std::path::PathBuf {
    if config::profile_is_active() {
        return config::cli_dir().join("config.json");
    }
    // 如果当前工作目录或可执行文件目录已有 config.json，沿用它（支持便携模式）
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("config.json");
        if p.exists() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("config.json");
            if p.exists() {
                return p;
            }
        }
    }
    // 默认写入 ~/.wx-cli/config.json（与 load_config 的最终查找路径保持一致）
    config::cli_dir().join("config.json")
}
