use std::{path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{StatusCode, header::RANGE};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    process::Command,
};

use crate::{
    api::{BiliClient, DashStream, codec_name},
    cli::{DownloadArgs, DownloadMode},
};

pub async fn run(client: &BiliClient, args: DownloadArgs) -> Result<()> {
    let info = client.video_info(&args.input).await?;
    let page = info.page(args.page)?;
    let bvid = info.page_bvid(page);
    let requested_qn = requested_qn(&args.quality);
    let play = client
        .play_info_for_page(bvid, page.cid, page.ep_id, requested_qn)
        .await?;
    let (video, audio, quality_description) = play.select_streams(&args.quality, args.codec)?;

    fs::create_dir_all(&args.output_dir)
        .await
        .with_context(|| format!("无法创建输出目录 {}", args.output_dir.display()))?;
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
        merge_with_ffmpeg(
            &args.ffmpeg,
            &video_path,
            &audio_path,
            &merged_path,
            args.force,
        )
        .await?;
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
    if destination.exists() && !force {
        println!("{label}已存在，跳过：{}", destination.display());
        return Ok(());
    }
    if force && destination.exists() {
        fs::remove_file(destination)
            .await
            .with_context(|| format!("无法覆盖已有文件 {}", destination.display()))?;
    }

    let partial = destination.with_extension("m4s.part");
    if force && partial.exists() {
        fs::remove_file(&partial)
            .await
            .with_context(|| format!("无法清理临时文件 {}", partial.display()))?;
    }
    let mut last_error = None;
    for url in stream.urls() {
        // 每次切换备用 CDN 都重新读取偏移，避免前一个 CDN 中途失败后重复追加数据。
        let existing = fs::metadata(&partial)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        match try_download_url(
            client,
            url,
            referer,
            destination,
            &partial,
            label,
            progress,
            existing,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("{label}没有可用下载地址")))
}

#[allow(clippy::too_many_arguments)]
async fn try_download_url(
    client: &BiliClient,
    url: &str,
    referer: &str,
    destination: &Path,
    partial: &Path,
    label: &str,
    progress: &MultiProgress,
    existing: u64,
) -> Result<()> {
    let mut request = client.media_get(url, referer);
    if existing > 0 {
        request = request.header(RANGE, format!("bytes={existing}-"));
    }
    let response = request.send().await.context("连接 CDN 失败")?;
    if !response.status().is_success() {
        bail!("CDN 返回 HTTP {}", response.status());
    }
    let resumed = existing > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
    let initial = if resumed { existing } else { 0 };
    let total = response.content_length().map(|length| length + initial);
    let bar = progress.add(make_progress_bar(label, total));
    bar.set_position(initial);

    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if resumed {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(partial)
        .await
        .with_context(|| format!("无法写入 {}", partial.display()))?;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("下载流中断")?;
        file.write_all(&chunk).await.context("写入下载文件失败")?;
        bar.inc(chunk.len() as u64);
    }
    file.flush().await.context("刷新下载文件失败")?;
    drop(file);
    bar.finish_with_message(format!("{label}完成"));

    if destination.exists() {
        fs::remove_file(destination).await?;
    }
    fs::rename(partial, destination)
        .await
        .with_context(|| format!("无法完成文件 {}", destination.display()))?;
    Ok(())
}

fn make_progress_bar(label: &str, total: Option<u64>) -> ProgressBar {
    let bar = total
        .map(ProgressBar::new)
        .unwrap_or_else(ProgressBar::new_spinner);
    let style = if total.is_some() {
        ProgressStyle::with_template(
            "{prefix:.bold} [{bar:32.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}",
        )
    } else {
        ProgressStyle::with_template("{prefix:.bold} {spinner} {bytes} {bytes_per_sec}")
    }
    .expect("static progress template")
    .progress_chars("=>-");
    bar.set_style(style);
    bar.set_prefix(label.to_owned());
    bar
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
                "无法启动 FFmpeg `{}`；请安装 FFmpeg 或通过 --ffmpeg 指定路径",
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

fn sanitize_filename(value: &str) -> String {
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
