# BilDowner

[![Release](https://github.com/Booklight12/BilDowner/actions/workflows/release.yml/badge.svg)](https://github.com/Booklight12/BilDowner/actions/workflows/release.yml)
[![License: BSD-2-Clause](https://img.shields.io/badge/License-BSD--2--Clause-blue.svg)](LICENSE)

BilDowner 是一个纯 Rust 命令行 Bilibili 视频下载器，不包含前端页面。它参考本机 Edge 扩展“bilibili哔哩哔哩下载助手”3.0.4 的下载链路：通过 `x/player/wbi/playurl` 获取 DASH 流，分别下载视频和音频，再调用 FFmpeg 以 `copy` 模式封装为 MP4，不进行有损重新编码。

当前版本：**1.0.0 Alpha**。

请只下载你有权保存的内容。本工具不会绕过 DRM、付费或账号权限；接口返回的清晰度取决于当前账号本身具有的播放权限。

## 功能

- 终端二维码登录，或手工导入 Cookie
- Windows 下使用 DPAPI 按当前 Windows 用户加密保存 Cookie
- 查看和选择分 P；支持 BV/AV 视频以及 `ep`/`ss` 番剧、影视链接
- 支持 `360p`、`480p`、`720p`、`1080p`、`1080p+`、`1080p60`、`4k`、`hdr`、`dolby`、`8k` 和原始 `qn`
- 可选择 AVC/H.264、HEVC/H.265 或 AV1；默认优先兼容性较好的 AVC
- `merged`、`separate`、`both` 三种输出模式
- 视频与音频并行下载，支持 `.part` 文件续传和 DASH 备用 CDN

## 环境

- 支持 edition 2024 的当前稳定版 Rust
- 合并模式需要 `ffmpeg` 在 `PATH` 中；分离模式不需要 FFmpeg

## 使用

```powershell
# 编译
cargo build --release

# 推荐：扫码登录
cargo run -- auth qr

# 检查登录状态
cargo run -- auth status

# 也可以从标准输入导入浏览器 Cookie，避免出现在命令历史中
Get-Clipboard | cargo run -- auth set

# 查看分 P、清晰度和编码
cargo run -- info "https://www.bilibili.com/video/BV1xx411c7mD"
cargo run -- info BV1xx411c7mD --page 1
cargo run -- info "https://www.bilibili.com/bangumi/play/ss43164" --page 2

# 默认下载最高可用清晰度，同时保留分离流和合并 MP4
cargo run -- download BV1xx411c7mD

# 选择清晰度、分 P、编码和输出模式
cargo run -- download BV1xx411c7mD --page 2 --quality 1080p --codec avc --mode both
cargo run -- download BV1xx411c7mD --quality 4k --codec hevc --mode merged
cargo run -- download BV1xx411c7mD --quality 720p --mode separate --output-dir .\downloads
cargo run -- download ep693247 --quality 1080p --mode both
```

输出模式：

- `merged`：输出带音频的 `.mp4`，成功后删除临时视频/音频流
- `separate`：输出 `.video.m4s` 与 `.audio.m4s`
- `both`：同时保留上述三种文件，也是默认值

如 FFmpeg 不在 `PATH` 中：

```powershell
cargo run -- download BV1xx411c7mD --ffmpeg C:\Tools\ffmpeg\bin\ffmpeg.exe
```

Cookie 默认保存在 `%APPDATA%\BilDowner\cookie.dat`，Windows 下通过 DPAPI 加密，只有保存它的 Windows 用户可以解密。可用 `BILDOWNER_COOKIE_FILE` 覆盖路径：

```powershell
$env:BILDOWNER_COOKIE_FILE = 'D:\private\bildowner-cookie.dat'
cargo run -- auth status
cargo run -- auth clear
```

## 实现说明

1. 使用 `x/web-interface/view` 解析 BV/AV 号，使用 `pgc/view/web/season` 解析 `ep`/`ss` 剧集、标题和 `cid`。
2. 从 `x/web-interface/nav` 获取当前 WBI 图片密钥，生成 `wts` 和 `w_rid` 签名，减少 CLI 请求被 412 拒绝的情况。
3. 普通视频使用 WBI playurl，番剧/影视使用 PGC playurl，并通过 `qn=<清晰度>&fnval=4048&fourk=1` 请求 DASH；如果接口标记为 DRM 流则明确拒绝下载。
4. 从同一清晰度的 `dash.video` 中按编码选择视频流，从 `dash.audio` 选择最高码率标准音频流。
5. 下载时携带 Bilibili Referer、浏览器 User-Agent 和已保存 Cookie；主 CDN 失败时依次尝试备用地址。
6. 合并时执行等价于 `ffmpeg -i video.m4s -i audio.m4s -map 0:v:0 -map 1:a:0 -c copy output.mp4` 的命令。

站点接口可能变化；遇到问题时先运行 `cargo run -- info <链接>`，它会给出当前账号实际可用的清晰度和编码。

## 开源许可证

本项目以 [BSD 2-Clause License](LICENSE) 开源，Copyright (c) 2026 SorMaze。
