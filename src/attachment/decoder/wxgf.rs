//! WeChat 4 WXGF/WXAM image container support.
//!
//! `wxgf` is not a normal image format. It is a private WeChat container whose
//! largest data partition is usually an Annex B HEVC bitstream. We keep the
//! parser tiny: find HEVC start codes after the WXGF header, validate the
//! 4-byte big-endian length immediately before the start code, then hand the
//! largest partition to ffmpeg.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const WXGF_MAGIC: &[u8; 4] = b"wxgf";
const FFMPEG_ENV: &str = "WX_FFMPEG";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WxgfPartition {
    pub offset: usize,
    /// Partition byte length, including the HEVC start code at `offset`.
    pub size: usize,
    pub ratio: f64,
}

#[derive(Debug)]
pub struct WxgfJpeg {
    pub data: Vec<u8>,
    pub partition: WxgfPartition,
    pub ffmpeg: String,
}

struct TempPaths {
    input: PathBuf,
    output: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.input);
        let _ = std::fs::remove_file(&self.output);
    }
}

/// Return the largest HEVC Annex B partition inside a WXGF/WXAM container.
pub fn largest_partition(data: &[u8]) -> Result<WxgfPartition> {
    if data.len() < 15 || &data[..4] != WXGF_MAGIC {
        bail!("invalid WXGF image container");
    }

    let header_len = data[4] as usize;
    if header_len >= data.len() {
        bail!("invalid WXGF header length {}", header_len);
    }

    for pattern in [&[0x00, 0x00, 0x00, 0x01][..], &[0x00, 0x00, 0x01][..]] {
        let mut partitions = Vec::new();
        let mut rel_offset = 0usize;

        while header_len + rel_offset < data.len() {
            let search_from = header_len + rel_offset;
            let Some(idx) = find_subslice(&data[search_from..], pattern) else {
                break;
            };
            let abs_idx = search_from + idx;
            if abs_idx < 4 {
                rel_offset = rel_offset.saturating_add(idx + 1);
                continue;
            }

            let size = u32::from_be_bytes(data[abs_idx - 4..abs_idx].try_into().unwrap()) as usize;
            if size > 0 && abs_idx.checked_add(size).is_some_and(|end| end <= data.len()) {
                partitions.push(WxgfPartition {
                    offset: abs_idx,
                    size,
                    ratio: size as f64 / data.len() as f64,
                });
                rel_offset = abs_idx - header_len + size;
            } else {
                rel_offset = abs_idx - header_len + 1;
            }
        }

        if let Some(max) = partitions.into_iter().max_by_key(|p| p.size) {
            return Ok(max);
        }
    }

    bail!("WXGF image has no valid HEVC partition")
}

/// Convert a WXGF/WXAM image to JPEG through ffmpeg.
///
/// The ffmpeg path is resolved from `WX_FFMPEG`, then falls back to `ffmpeg` in
/// PATH. This avoids adding Python or native HEVC decoder dependencies.
pub fn transcode_to_jpeg(data: &[u8]) -> Result<WxgfJpeg> {
    let partition = largest_partition(data)?;
    let hevc = &data[partition.offset..partition.offset + partition.size];
    let ffmpeg = std::env::var(FFMPEG_ENV).unwrap_or_else(|_| "ffmpeg".to_string());
    let paths = temp_paths();

    std::fs::write(&paths.input, hevc)
        .with_context(|| format!("写出 WXGF/HEVC 临时输入失败：{}", paths.input.display()))?;

    let output = Command::new(&ffmpeg)
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("hevc")
        .arg("-i")
        .arg(&paths.input)
        .arg("-vframes")
        .arg("1")
        .arg("-c:v")
        .arg("mjpeg")
        .arg("-q:v")
        .arg("4")
        .arg(&paths.output)
        .output()
        .with_context(|| {
            format!(
                "启动 ffmpeg 失败；请安装 ffmpeg 或用 {FFMPEG_ENV} 指定路径，或用 wx extract --raw 导出原始 WXGF"
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffmpeg 转码 WXGF/HEVC 失败：{}",
            stderr.trim().chars().take(800).collect::<String>()
        );
    }

    let data = std::fs::read(&paths.output)
        .with_context(|| format!("读取 ffmpeg 输出失败：{}", paths.output.display()))?;
    if data.is_empty() {
        bail!("ffmpeg 转码 WXGF/HEVC 成功但没有输出 JPEG 数据");
    }

    Ok(WxgfJpeg {
        data,
        partition,
        ffmpeg,
    })
}

fn temp_paths() -> TempPaths {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = format!("wx-cli-wxgf-{}-{}-{}", std::process::id(), nanos, seq);
    let dir = std::env::temp_dir();
    TempPaths {
        input: dir.join(format!("{}.hevc", stem)),
        output: dir.join(format!("{}.jpg", stem)),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_largest_partition() {
        let mut data = b"wxgf".to_vec();
        data.push(19); // header length
        data.extend_from_slice(&[0; 14]);

        data.extend_from_slice(&8u32.to_be_bytes());
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(&[1, 2, 3, 4]);

        let second_offset = data.len() + 4;
        data.extend_from_slice(&12u32.to_be_bytes());
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(&[5, 6, 7, 8, 9, 10, 11, 12]);

        let p = largest_partition(&data).unwrap();
        assert_eq!(p.offset, second_offset);
        assert_eq!(p.size, 12);
    }

    #[test]
    fn rejects_non_wxgf() {
        let err = largest_partition(b"not wxgf").unwrap_err().to_string();
        assert!(err.contains("WXGF"));
    }
}
