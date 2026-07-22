use std::{path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use indicatif::MultiProgress;
use tokio::{fs, process::Command};

use crate::{
    api::{BiliClient, DashStream, VideoInfo, VideoPage, codec_name, print_page_selection_notice},
    cli::{DownloadArgs, DownloadMode},
    http_download, mux,
};

pub async fn run(client: &BiliClient, args: DownloadArgs) -> Result<()> {
    let info = client.video_info(&args.input).await?;
    let selection = info.resolve_pages(&args.input, args.page.as_deref())?;
    print_page_selection_notice(&info, args.page.as_deref(), &selection, "下载");

    fs::create_dir_all(&args.output_dir)
        .await
        .with_context(|| format!("无法创建输出目录 {}", args.output_dir.display()))?;
    for page_number in selection.pages {
        let page = info.page(page_number)?;
        download_page(client, &info, page, &args).await?;
    }
    Ok(())
}

async fn download_page(
    client: &BiliClient,
    info: &VideoInfo,
    page: &VideoPage,
    args: &DownloadArgs,
) -> Result<()> {
    let bvid = info.page_bvid(page);
    let requested_qn = requested_qn(&args.quality);
    let play = client
        .play_info_for_page(bvid, page.cid, page.ep_id, requested_qn)
        .await?;
    let (video, audio, quality_description) = play.select_streams(&args.quality, args.codec)?;
    if args.mode.needs_merge() && args.ffmpeg.is_none() && video.codecid != 7 {
        bail!(
            "内置 MP4 合并目前仅支持 AVC/H.264；请使用 `--codec avc`、传入 `--ffmpeg`，或用 `--mode separate` 保留 {} 分离流",
            codec_name(video.codecid, &video.codecs)
        );
    }

    let base = output_base(&info.title, page.page, &page.part, &quality_description);
    let video_path = args.output_dir.join(format!("{base}.video.m4s"));
    let audio_path = args.output_dir.join(format!("{base}.audio.m4s"));
    let merged_path = args.output_dir.join(format!("{base}.mp4"));
    let referer = page
        .ep_id
        .map(|ep_id| format!("https://www.bilibili.com/bangumi/play/ep{ep_id}"))
        .unwrap_or_else(|| format!("https://www.bilibili.com/video/{bvid}?p={}", page.page));

    println!(
        "准备下载：{} / P{} {}\n清晰度：{}，视频编码：{}，{}x{} {}\n音频码率：约 {} kbps",
        info.title,
        page.page,
        page.part,
        quality_description,
        codec_name(video.codecid, &video.codecs),
        video.width,
        video.height,
        video.frame_rate,
        audio.bandwidth / 1000
    );

    let progress = MultiProgress::new();
    let video_download = download_stream(
        client,
        video,
        &referer,
        &video_path,
        "视频",
        &progress,
        args.force,
    );
    let audio_download = download_stream(
        client,
        audio,
        &referer,
        &audio_path,
        "音频",
        &progress,
        args.force,
    );
    tokio::try_join!(video_download, audio_download)?;

    if args.mode.needs_merge() {
        if let Some(ffmpeg) = args.ffmpeg.as_deref() {
            merge_with_ffmpeg(ffmpeg, &video_path, &audio_path, &merged_path, args.force).await?;
        } else {
            merge_with_rust(&video_path, &audio_path, &merged_path, args.force).await?;
        }
        println!("合并文件：{}", merged_path.display());
    }
    if args.mode.keeps_separate() {
        println!(
            "分离视频：{}\n分离音频：{}",
            video_path.display(),
            audio_path.display()
        );
    } else if args.mode == DownloadMode::Merged {
        fs::remove_file(&video_path).await.ok();
        fs::remove_file(&audio_path).await.ok();
    }
    Ok(())
}

fn requested_qn(value: &str) -> u32 {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "best" | "最高" => 127,
        "240p" => 6,
        "360p" => 16,
        "480p" => 32,
        "720p" => 64,
        "720p60" => 74,
        "1080p" => 80,
        "1080p+" | "1080phigh" => 112,
        "1080p60" => 116,
        "4k" | "2160p" => 120,
        "hdr" => 125,
        "dolby" | "dolbyvision" | "杜比视界" => 126,
        "8k" | "4320p" => 127,
        _ => normalized.parse().unwrap_or(127),
    }
}

async fn download_stream(
    client: &BiliClient,
    stream: &DashStream,
    referer: &str,
    destination: &Path,
    label: &str,
    progress: &MultiProgress,
    force: bool,
) -> Result<()> {
    let urls = stream.urls().map(str::to_owned).collect::<Vec<_>>();
    http_download::download_urls(&urls, destination, label, progress, force, true, |url| {
        client.media_get(url, referer)
    })
    .await
}

async fn merge_with_rust(video: &Path, audio: &Path, output: &Path, force: bool) -> Result<()> {
    if output.exists() && !force {
        println!("合并文件已存在，跳过：{}", output.display());
        return Ok(());
    }
    let temporary = output.with_extension("tmp.mp4");
    if temporary.exists() {
        fs::remove_file(&temporary)
            .await
            .with_context(|| format!("无法清理 MP4 临时文件 {}", temporary.display()))?;
    }
    println!("正在用内置 MP4 封装器无重新编码合并……");
    let video = video.to_owned();
    let audio = audio.to_owned();
    let temporary_for_task = temporary.clone();
    let result =
        tokio::task::spawn_blocking(move || mux::mux_avc_aac(&video, &audio, &temporary_for_task))
            .await
            .context("内置 MP4 合并任务异常终止")?;
    if let Err(error) = result {
        fs::remove_file(&temporary).await.ok();
        return Err(error);
    }
    if output.exists() {
        fs::remove_file(output)
            .await
            .with_context(|| format!("无法覆盖合并文件 {}", output.display()))?;
    }
    fs::rename(&temporary, output)
        .await
        .with_context(|| format!("无法完成合并文件 {}", output.display()))?;
    Ok(())
}

async fn merge_with_ffmpeg(
    ffmpeg: &Path,
    video: &Path,
    audio: &Path,
    output: &Path,
    force: bool,
) -> Result<()> {
    if output.exists() && !force {
        println!("合并文件已存在，跳过：{}", output.display());
        return Ok(());
    }
    let temporary = output.with_extension("tmp.mp4");
    if temporary.exists() {
        fs::remove_file(&temporary)
            .await
            .with_context(|| format!("无法清理 FFmpeg 临时文件 {}", temporary.display()))?;
    }
    println!("正在用 FFmpeg 无重新编码合并……");
    let status = Command::new(ffmpeg)
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-i")
        .arg(video)
        .arg("-i")
        .arg(audio)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a:0")
        .arg("-c")
        .arg("copy")
        .arg(&temporary)
        .stdin(Stdio::null())
        .status()
        .await
        .with_context(|| {
            format!(
                "无法启动 FFmpeg `{}`；请安装 FFmpeg 或传入正确的 --ffmpeg 路径",
                ffmpeg.display()
            )
        })?;
    if !status.success() {
        fs::remove_file(&temporary).await.ok();
        bail!("FFmpeg 合并失败，退出码：{status}");
    }
    if output.exists() {
        fs::remove_file(output)
            .await
            .with_context(|| format!("无法覆盖合并文件 {}", output.display()))?;
    }
    fs::rename(&temporary, output)
        .await
        .with_context(|| format!("无法完成合并文件 {}", output.display()))?;
    Ok(())
}

fn output_base(title: &str, page: usize, part: &str, quality: &str) -> String {
    let raw = if part == title || part.is_empty() {
        format!("{title}-P{page}-{quality}")
    } else {
        format!("{title}-P{page}-{part}-{quality}")
    };
    sanitize_filename(&raw)
}

pub(crate) fn sanitize_filename(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    result = result.trim().trim_end_matches(['.', ' ']).to_owned();
    if result.len() > 160 {
        result = result.chars().take(160).collect();
    }
    if result.is_empty() {
        "bilibili-video".to_owned()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{requested_qn, sanitize_filename};

    #[test]
    fn maps_quality_to_qn() {
        assert_eq!(requested_qn("1080p+"), 112);
        assert_eq!(requested_qn("4K"), 120);
    }

    #[test]
    fn removes_windows_filename_characters() {
        assert_eq!(sanitize_filename("a:b/c*?"), "a_b_c__");
    }
}
