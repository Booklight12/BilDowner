use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "bildowner",
    version,
    about = "纯命令行 Bilibili DASH 视频下载器"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 登录、保存或清除 Bilibili Cookie
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// 查看分 P、可用清晰度和编码
    Info {
        /// BV/AV/ep/ss 号或 Bilibili 视频链接
        input: String,
        /// 分 P 序号，从 1 开始
        #[arg(short, long, default_value_t = 1)]
        page: usize,
    },
    /// 下载视频
    Download(DownloadArgs),
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// 在终端显示二维码，扫码登录并加密保存 Cookie
    Qr,
    /// 手工保存 Cookie；不传参数时从标准输入读取
    Set {
        /// Cookie 字符串，例如 SESSDATA=...; bili_jct=...
        #[arg(long, conflicts_with = "cookie_file")]
        cookie: Option<String>,
        /// 从文件读取 Cookie
        #[arg(long, value_name = "PATH", conflicts_with = "cookie")]
        cookie_file: Option<PathBuf>,
    },
    /// 检查本地 Cookie 和当前登录状态
    Status,
    /// 删除本地保存的 Cookie
    Clear,
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    /// BV/AV/ep/ss 号或 Bilibili 视频链接
    pub input: String,

    /// 清晰度：best、360p、480p、720p、1080p、1080p+、1080p60、4k、hdr、dolby、8k 或数值 qn
    #[arg(short, long, default_value = "best")]
    pub quality: String,

    /// 分 P 序号，从 1 开始
    #[arg(short, long, default_value_t = 1)]
    pub page: usize,

    /// 视频编码偏好
    #[arg(long, value_enum, default_value_t = CodecChoice::Auto)]
    pub codec: CodecChoice,

    /// 输出模式；both 会同时保留分离流和合并文件
    #[arg(short, long, value_enum, default_value_t = DownloadMode::Both)]
    pub mode: DownloadMode,

    /// 输出目录
    #[arg(short, long, default_value = "downloads")]
    pub output_dir: PathBuf,

    /// FFmpeg 可执行文件路径
    #[arg(long, default_value = "ffmpeg")]
    pub ffmpeg: PathBuf,

    /// 覆盖已经存在的文件
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CodecChoice {
    /// 优先 AVC/H.264，其次 HEVC、AV1
    Auto,
    Avc,
    Hevc,
    Av1,
}

impl CodecChoice {
    pub fn codecid(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::Avc => Some(7),
            Self::Hevc => Some(12),
            Self::Av1 => Some(13),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DownloadMode {
    /// 只输出含音频的 MP4
    Merged,
    /// 只输出 video.m4s 和 audio.m4s
    Separate,
    /// 同时输出分离流和合并 MP4
    Both,
}

impl DownloadMode {
    pub fn needs_merge(self) -> bool {
        matches!(self, Self::Merged | Self::Both)
    }

    pub fn keeps_separate(self) -> bool {
        matches!(self, Self::Separate | Self::Both)
    }
}
