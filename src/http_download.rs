use std::path::Path;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{StatusCode, header::RANGE};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};

pub async fn download_urls<F>(
    urls: &[String],
    destination: &Path,
    label: &str,
    progress: &MultiProgress,
    force: bool,
    resume_across_urls: bool,
    mut make_request: F,
) -> Result<()>
where
    F: FnMut(&str) -> reqwest::RequestBuilder,
{
    if destination.exists() && !force {
        println!("{label}已存在，跳过：{}", destination.display());
        return Ok(());
    }
    if force && destination.exists() {
        fs::remove_file(destination)
            .await
            .with_context(|| format!("无法覆盖已有文件 {}", destination.display()))?;
    }

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("下载文件名不是有效 UTF-8")?;
    let partial = destination.with_file_name(format!("{file_name}.part"));
    if force && partial.exists() {
        fs::remove_file(&partial)
            .await
            .with_context(|| format!("无法清理临时文件 {}", partial.display()))?;
    }

    let mut last_error = None;
    for (index, url) in urls.iter().enumerate() {
        if index > 0 && !resume_across_urls && partial.exists() {
            fs::remove_file(&partial)
                .await
                .with_context(|| format!("无法清理不兼容的临时文件 {}", partial.display()))?;
        }
        // 每次切换备用地址都重新读取偏移，避免前一个地址中途失败后重复追加数据。
        let existing = fs::metadata(&partial)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        match try_download_url(
            make_request(url),
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

async fn try_download_url(
    request: reqwest::RequestBuilder,
    destination: &Path,
    partial: &Path,
    label: &str,
    progress: &MultiProgress,
    existing: u64,
) -> Result<()> {
    let request = if existing > 0 {
        request.header(RANGE, format!("bytes={existing}-"))
    } else {
        request
    };
    let response = request.send().await.context("连接下载地址失败")?;
    if !response.status().is_success() {
        bail!("下载地址返回 HTTP {}", response.status());
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html") || value.contains("application/json"))
    {
        bail!("下载地址返回的不是媒体文件");
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
