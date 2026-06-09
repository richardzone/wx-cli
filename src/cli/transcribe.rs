use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::output::{print_value, resolve};
use super::transport;
use crate::ipc::Request;

/// `wx transcribe` — 从语音 attachment_id 导出音频并调用本机 ASR。
///
/// Pipeline:
/// 1. daemon `Extract` 导出 WeChat 原始语音 bytes
/// 2. SILK v3: 规整 `#!SILK` header → decoder 输出 s16le PCM
/// 3. ffmpeg 转为 whisper.cpp 需要的 16k mono WAV
/// 4. whisper-cli 做本地 ASR
pub fn cmd_transcribe(
    attachment_id: String,
    model: Option<PathBuf>,
    whisper_bin: Option<PathBuf>,
    silk_decoder: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
    language: String,
    keep_temp: bool,
    json_out: bool,
) -> Result<()> {
    let model = resolve_required_model(model)?;
    let whisper_bin = resolve_tool(
        whisper_bin,
        "WX_WHISPER_BIN",
        &["whisper-cli"],
        "找不到 whisper.cpp 的 whisper-cli；请用 --whisper-bin 指定路径，或设置 WX_WHISPER_BIN",
    )?;
    let ffmpeg = resolve_tool(
        ffmpeg,
        "WX_FFMPEG",
        &["ffmpeg"],
        "找不到 ffmpeg；请安装 ffmpeg，或用 --ffmpeg 指定路径",
    )?;

    let work = WorkDir::new(keep_temp)?;
    let raw_path = work.path.join("voice.aud");
    let silk_path = work.path.join("voice.silk");
    let pcm_path = work.path.join("voice.pcm");
    let wav_path = work.path.join("voice.wav");

    let extract_report = extract_voice(&attachment_id, &raw_path)?;
    let kind = extract_report
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind != "voice" {
        return Err(anyhow!(
            "attachment_id 不是语音资源（kind={}），请先用 `wx attachments CHAT --kind voice` 获取语音 ID",
            kind
        ));
    }

    let raw_bytes = std::fs::read(&raw_path)
        .with_context(|| format!("读取语音文件失败：{}", raw_path.display()))?;
    let format = detect_audio_format(
        extract_report
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        &raw_bytes,
        &raw_path,
    );

    let mut silk_header_offset: Option<usize> = None;
    let decode_stage = if format == "silk" {
        let silk_decoder = resolve_tool(
            silk_decoder,
            "WX_SILK_DECODER",
            &["silk-decoder", "silk_v3_decoder", "silk_decoder"],
            "找不到 SILK v3 decoder；请用 --silk-decoder 指定 kn007/silk-v3-decoder 的 silk/decoder 路径，或设置 WX_SILK_DECODER",
        )?;
        silk_header_offset = Some(write_normalized_silk(&raw_bytes, &silk_path)?);
        run_silk_decoder(&silk_decoder, &silk_path, &pcm_path)?;
        run_ffmpeg_pcm_to_wav(&ffmpeg, &pcm_path, &wav_path)?;
        json!({
            "input_format": "silk",
            "silk_header_offset": silk_header_offset,
            "silk_decoder": silk_decoder.display().to_string(),
        })
    } else {
        run_ffmpeg_audio_to_wav(&ffmpeg, &raw_path, &wav_path)?;
        json!({
            "input_format": format,
            "silk_header_offset": silk_header_offset,
        })
    };

    let whisper = run_whisper(&whisper_bin, &model, &wav_path, &language)?;
    let transcript = clean_whisper_stdout(&whisper.stdout);

    let mut report = json!({
        "transcript": transcript,
        "language": language,
        "engine": "whisper.cpp",
        "model": model.display().to_string(),
        "whisper_bin": whisper_bin.display().to_string(),
        "ffmpeg": ffmpeg.display().to_string(),
        "audio": {
            "source": extract_report.get("source").cloned(),
            "format": format,
            "decoder": extract_report.get("decoder").cloned(),
            "output_size": extract_report.get("output_size").cloned(),
        },
        "decode": decode_stage,
        "whisper": {
            "stderr": whisper.stderr.trim(),
        },
        "kept_temp": keep_temp,
    });

    if keep_temp {
        report["temp_dir"] = json!(work.path.display().to_string());
        report["files"] = json!({
            "raw": raw_path.display().to_string(),
            "silk": if silk_path.exists() { Some(silk_path.display().to_string()) } else { None },
            "pcm": if pcm_path.exists() { Some(pcm_path.display().to_string()) } else { None },
            "wav": wav_path.display().to_string(),
        });
    }

    print_value(&report, &resolve(json_out))
}

fn extract_voice(attachment_id: &str, raw_path: &Path) -> Result<Value> {
    let resp = transport::send(Request::Extract {
        attachment_id: attachment_id.to_string(),
        output: raw_path.display().to_string(),
        overwrite: true,
    })?;
    set_private_file_permissions(raw_path)?;
    Ok(resp.data)
}

fn resolve_required_model(model: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = model {
        return require_existing_file(path, "--model");
    }
    if let Ok(path) = std::env::var("WX_WHISPER_MODEL") {
        return require_existing_file(PathBuf::from(path), "WX_WHISPER_MODEL");
    }
    Err(anyhow!(
        "缺少 whisper.cpp 模型路径；请传 --model /path/to/ggml-large-v3-turbo.bin，或设置 WX_WHISPER_MODEL"
    ))
}

fn resolve_tool(
    explicit: Option<PathBuf>,
    env_name: &str,
    candidates: &[&str],
    missing_msg: &str,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return require_existing_file(path, env_name);
    }
    if let Ok(path) = std::env::var(env_name) {
        return require_existing_file(PathBuf::from(path), env_name);
    }
    for candidate in candidates {
        if let Some(path) = find_in_path(candidate) {
            return Ok(path);
        }
    }
    Err(anyhow!(missing_msg.to_string()))
}

fn require_existing_file(path: PathBuf, label: &str) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(anyhow!("{} 指向的文件不存在：{}", label, path.display()))
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn detect_audio_format<'a>(reported: &'a str, bytes: &[u8], path: &Path) -> &'a str {
    if find_subslice_prefix(bytes, b"#!SILK", 8).is_some() {
        return "silk";
    }
    if bytes.starts_with(b"#!AMR") {
        return "amr";
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return "wav";
    }
    if bytes.starts_with(b"ID3") || bytes.starts_with(&[0xFF, 0xFB]) {
        return "mp3";
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return "m4a";
    }
    if !reported.is_empty() && reported != "bin" && reported != "dat" {
        return reported;
    }
    match path.extension().and_then(OsStr::to_str).unwrap_or_default() {
        "amr" => "amr",
        "wav" => "wav",
        "m4a" => "m4a",
        "mp3" => "mp3",
        "silk" | "slk" => "silk",
        _ => "bin",
    }
}

fn write_normalized_silk(bytes: &[u8], silk_path: &Path) -> Result<usize> {
    let offset = find_subslice_prefix(bytes, b"#!SILK", 8).ok_or_else(|| {
        anyhow!("语音报告为 SILK，但前 8 字节内找不到 #!SILK header，无法调用 SILK decoder")
    })?;
    write_private_file(silk_path, &bytes[offset..])
        .with_context(|| format!("写出 SILK 中间文件失败：{}", silk_path.display()))?;
    Ok(offset)
}

fn find_subslice_prefix(haystack: &[u8], needle: &[u8], max_offset: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let end = haystack.len().saturating_sub(needle.len()).min(max_offset);
    (0..=end).find(|&idx| &haystack[idx..idx + needle.len()] == needle)
}

fn run_silk_decoder(decoder: &Path, silk_path: &Path, pcm_path: &Path) -> Result<()> {
    let output = Command::new(decoder)
        .arg(silk_path)
        .arg(pcm_path)
        .output()
        .with_context(|| format!("启动 SILK decoder 失败：{}", decoder.display()))?;
    if !output.status.success() || !pcm_path.is_file() {
        return Err(anyhow!(
            "SILK decoder 失败：{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    set_private_file_permissions(pcm_path)?;
    Ok(())
}

fn run_ffmpeg_pcm_to_wav(ffmpeg: &Path, pcm_path: &Path, wav_path: &Path) -> Result<()> {
    run_command(
        Command::new(ffmpeg)
            .arg("-y")
            .arg("-f")
            .arg("s16le")
            .arg("-ar")
            .arg("24000")
            .arg("-ac")
            .arg("1")
            .arg("-i")
            .arg(pcm_path)
            .arg("-ar")
            .arg("16000")
            .arg("-ac")
            .arg("1")
            .arg("-c:a")
            .arg("pcm_s16le")
            .arg(wav_path),
        "ffmpeg PCM -> WAV",
    )?;
    set_private_file_permissions(wav_path)
}

fn run_ffmpeg_audio_to_wav(ffmpeg: &Path, input_path: &Path, wav_path: &Path) -> Result<()> {
    run_command(
        Command::new(ffmpeg)
            .arg("-y")
            .arg("-i")
            .arg(input_path)
            .arg("-ar")
            .arg("16000")
            .arg("-ac")
            .arg("1")
            .arg("-c:a")
            .arg("pcm_s16le")
            .arg(wav_path),
        "ffmpeg audio -> WAV",
    )?;
    set_private_file_permissions(wav_path)
}

fn run_whisper(
    whisper_bin: &Path,
    model: &Path,
    wav_path: &Path,
    language: &str,
) -> Result<CommandOutput> {
    let output = Command::new(whisper_bin)
        .arg("-m")
        .arg(model)
        .arg("-f")
        .arg(wav_path)
        .arg("-l")
        .arg(language)
        .arg("-nt")
        .arg("-np")
        .output()
        .with_context(|| format!("启动 whisper-cli 失败：{}", whisper_bin.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "whisper-cli 失败：{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn run_command(cmd: &mut Command, stage: &str) -> Result<()> {
    let output = cmd
        .output()
        .with_context(|| format!("启动 {} 失败", stage))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 失败：{}\n{}",
            stage,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("创建私有文件失败：{}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("写入私有文件失败：{}", path.display()))?;
    set_private_file_permissions(path)
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置文件权限失败：{}", path.display()))?;
    }
    Ok(())
}

fn clean_whisper_stdout(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

struct CommandOutput {
    stdout: String,
    stderr: String,
}

struct WorkDir {
    path: PathBuf,
    keep: bool,
}

impl WorkDir {
    fn new(keep: bool) -> Result<Self> {
        for attempt in 0..128u32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "wx-transcribe-{}-{}-{}",
                std::process::id(),
                nanos,
                attempt
            ));
            match create_private_dir(&path) {
                Ok(()) => {
                    return Ok(Self { path, keep });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("创建临时目录失败：{}", path.display()));
                }
            }
        }
        Err(anyhow!("创建临时目录失败：连续 128 次命名冲突"))
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_silk_header_after_wechat_prefix() {
        assert_eq!(
            find_subslice_prefix(b"\x02#!SILK_V3", b"#!SILK", 8),
            Some(1)
        );
        assert_eq!(find_subslice_prefix(b"#!SILK_V3", b"#!SILK", 8), Some(0));
    }

    #[test]
    fn clean_whisper_stdout_keeps_non_empty_lines() {
        assert_eq!(clean_whisper_stdout("\n  你好  \n\n世界\n"), "你好\n世界");
    }

    #[cfg(unix)]
    #[test]
    fn workdir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let work = WorkDir::new(true).unwrap();
        let mode = std::fs::metadata(&work.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&work.path).unwrap();
    }
}
