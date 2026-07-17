# BilDowner

[![Release](https://github.com/Booklight12/BilDowner/actions/workflows/release.yml/badge.svg)](https://github.com/Booklight12/BilDowner/actions/workflows/release.yml)
[![License: BSD-2-Clause](https://img.shields.io/badge/License-BSD--2--Clause-blue.svg)](LICENSE)

BilDowner 是一个纯 Rust 命令行 Bilibili / 抖音 / X 视频下载器，不包含前端页面。Bilibili 通过 `x/player/wbi/playurl` 获取 DASH 流，分别下载视频和音频，默认由内置 MP4 封装器逐样本合并 AVC/H.264 与 AAC 轨道；也可以显式选择 FFmpeg。两种方式都不进行有损重新编码。抖音通过分享页公开的 SSR 作品数据解析 MP4 播放流，优先下载无水印地址，并在不可用时自动回退到原始播放地址。X 下载器从官方 syndication JSON 解析 `video.twimg.com` MP4 变体，默认下载最高码率版本。
当前版本：**1.1.0 Alpha**。

请只下载你有权保存的内容。本工具不会绕过 DRM、付费或账号权限；接口返回的清晰度取决于当前账号本身具有的播放权限。

## 功能

- 终端二维码登录，或手工导入 Cookie
- Windows 下使用 DPAPI 按当前 Windows 用户加密保存 Cookie
- 查看和选择分 P；支持 BV/AV 视频以及 `ep`/`ss` 番剧、影视链接
- 支持 `360p`、`480p`、`720p`、`1080p`、`1080p+`、`1080p60`、`4k`、`hdr`、`dolby`、`8k` 和原始 `qn`
- 可选择 AVC/H.264、HEVC/H.265 或 AV1；默认优先兼容性较好的 AVC
- `merged`、`separate`、`both` 三种输出模式
- 视频与音频并行下载，支持 `.part` 文件续传和 DASH 备用 CDN
- 内置纯 Rust MP4 合并器，默认 AVC/H.264 + AAC 下载无需安装 FFmpeg；可通过 `--ffmpeg` 显式切换
- 支持抖音短分享链接和 `douyin.com/video/<作品 ID>` 链接，无需登录或额外插件
- 支持 `x.com` / `twitter.com` 帖子视频，默认最高码率，也可用 `--quality` 限制分辨率

## 环境

- 支持 edition 2024 的当前稳定版 Rust
- 默认不需要 FFmpeg；内置合并当前支持 AVC/H.264 + AAC
- HEVC/H.265、AV1 可使用 `--mode separate` 保留原始流，或通过 `--ffmpeg` 合并

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
cargo run -- download BV1xx411c7mD --quality 4k --codec hevc --mode merged --ffmpeg
cargo run -- download BV1xx411c7mD --quality 720p --mode separate --output-dir .\downloads
cargo run -- download ep693247 --quality 1080p --mode both

# 查看并下载抖音分享视频（抖音为已合并的单个 MP4）
cargo run -- info "https://v.douyin.com/KyuVvC8wEu4/"
cargo run -- download "https://v.douyin.com/KyuVvC8wEu4/"

# 查看并下载 X 帖子中的主视频（公开帖子无需登录）
cargo run -- info "https://x.com/Kimi_Moonshot/status/2077521842080817296"
cargo run -- download "https://x.com/Kimi_Moonshot/status/2077521842080817296"
cargo run -- download "https://x.com/Kimi_Moonshot/status/2077521842080817296" --quality 1080p
```

输出模式：

- `merged`：输出带音频的 `.mp4`，成功后删除临时视频/音频流
- `separate`：输出 `.video.m4s` 与 `.audio.m4s`
- `both`：同时保留上述三种文件，也是默认值

默认使用内置合并器。需要 FFmpeg 时可显式启用，或指定自定义路径：

```powershell
cargo run -- download BV1xx411c7mD --mode merged --ffmpeg
cargo run -- download BV1xx411c7mD --mode merged --ffmpeg C:\Tools\ffmpeg\bin\ffmpeg.exe
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
6. 默认合并时用纯 Rust 解析 fragmented MP4，复制 H.264/AAC 压缩样本、时间戳与关键帧信息，并重建普通 MP4 的 `mdat` 和样本索引；传入 `--ffmpeg` 时改用 FFmpeg 的 `copy` 模式封装。
7. 抖音链接先解析为作品 ID，再读取 `www.douyin.com/share/video/<作品 ID>` 页面内的 `_ROUTER_DATA`；下载器优先尝试 `play` 无水印地址，并以页面提供的 `playwm` 地址兜底。
8. X 链接提取帖子 ID 后请求官方 syndication JSON，读取首个视频的 MP4 变体；默认按码率选择最高档，指定 `--quality` 时选择不高于目标高度的最佳档位。Windows 使用系统 WinINet 网络路径，其他平台使用 Rustls。

站点接口可能变化；遇到问题时先运行 `cargo run -- info <链接>`。Bilibili 会给出当前账号实际可用的清晰度和编码，抖音会显示分享页公开的作品信息，X 会列出帖子首个视频的全部 MP4 档位。

## 开源许可证

本项目以 [BSD 2-Clause License](LICENSE) 开源，Copyright (c) 2026 SorMaze。
