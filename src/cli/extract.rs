use anyhow::Result;

use super::output::{print_value, resolve};
use super::transport;
use crate::ipc::Request;

/// `wx extract` — 把单个 `attachment_id` 对应的资源写到指定路径。
///
/// daemon 端：解析 `attachment_id` → 查 `message_resource.db` 拿 file md5 →
/// 在 `<wxchat_base>/msg/attach/...` 找资源文件。image 按 magic 分发到 v1/v2
/// 解码器，voice/audio POC 原样复制。
pub fn cmd_extract(
    attachment_id: String,
    output: String,
    overwrite: bool,
    json: bool,
) -> Result<()> {
    let req = Request::Extract {
        attachment_id,
        output,
        overwrite,
    };
    let resp = transport::send(req)?;
    print_value(&resp.data, &resolve(json))
}
