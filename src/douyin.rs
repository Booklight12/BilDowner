use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use indicatif::MultiProgress;
use regex::Regex;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, ORIGIN, REFERER};
use serde::Deserialize;
use url::Url;

use crate::{api::USER_AGENT, cli::DownloadArgs, download::sanitize_filename, http_download};

const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

pub fn is_douyin_input(input: &str) -> bool {
    Url::parse(input.trim())
        .ok()
        .and_then(|url| url.host_str().map(is_douyin_host))
        .unwrap_or(false)
}

pub async fn print_info(input: &str) -> Result<()> {
    let client = DouyinClient::new()?;
    let info = client.video_info(input).await?;
    println!(
        "{}\n作者：{}\n抖音作品 ID：{}\n时长：{}\n作品元数据尺寸：{}x{}\n下载格式：MP4（优先无水印，失败时自动使用原始播放地址）",
        info.description,
        info.author,
        info.id,
        format_duration(info.duration_ms),
        info.width,
        info.height
    );
    Ok(())
}

pub async fn download(args: DownloadArgs) -> Result<()> {
    if !args.quality.trim().eq_ignore_ascii_case("best")
        || !matches!(args.codec, crate::cli::CodecChoice::Auto)
    {
        println!("提示：抖音分享页只提供单个 MP4 播放流，已忽略清晰度和编码选项。");
    }

    let client = DouyinClient::new()?;
    let info = client.video_info(&args.input).await?;
    tokio::fs::create_dir_all(&args.output_dir)
        .await
        .with_context(|| format!("无法创建输出目录 {}", args.output_dir.display()))?;
    let destination = args.output_dir.join(format!("{}.mp4", output_base(&info)));
    let candidates = download_candidates(&info.play_urls);

    println!(
        "准备下载抖音视频：{}\n作者：{}\n时长：{}，作品元数据尺寸：{}x{}",
        info.description,
        info.author,
        format_duration(info.duration_ms),
        info.width,
        info.height
    );
    let progress = MultiProgress::new();
    http_download::download_urls(
        &candidates,
        &destination,
        "视频",
        &progress,
        args.force,
        false,
        |url| client.media_get(url, &info.id),
    )
    .await?;
    println!("视频文件：{}", destination.display());
    Ok(())
}

struct DouyinClient {
    client: reqwest::Client,
}

impl DouyinClient {
    fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(MOBILE_USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .context("无法创建抖音 HTTP 客户端")?;
        Ok(Self { client })
    }

    async fn video_info(&self, input: &str) -> Result<DouyinVideo> {
        let id = if let Some(id) = video_id_from_url(input)? {
            id
        } else {
            let response = self
                .page_get(input)
                .send()
                .await
                .context("解析抖音分享短链失败")?
                .error_for_status()
                .context("抖音分享短链返回 HTTP 错误")?;
            video_id_from_url(response.url().as_str())?
                .context("抖音分享链接跳转后没有找到视频作品 ID")?
        };

        let share_url = format!("https://www.douyin.com/share/video/{id}");
        let html = self
            .page_get(&share_url)
            .send()
            .await
            .context("请求抖音分享页失败")?
            .error_for_status()
            .context("抖音分享页返回 HTTP 错误")?
            .text()
            .await
            .context("读取抖音分享页失败")?;
        parse_router_data(&html, &id)
    }

    fn page_get(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8",
            )
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
            .header(REFERER, "https://www.douyin.com/")
    }

    fn media_get(&self, url: &str, id: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header(ACCEPT, "*/*")
            .header(ORIGIN, "https://www.douyin.com")
            .header(REFERER, format!("https://www.douyin.com/video/{id}"))
            .header(reqwest::header::USER_AGENT, USER_AGENT)
    }
}

#[derive(Debug)]
struct DouyinVideo {
    id: String,
    description: String,
    author: String,
    duration_ms: u64,
    width: u32,
    height: u32,
    play_urls: Vec<String>,
}

#[derive(Deserialize)]
struct RouterData {
    #[serde(rename = "loaderData")]
    loader_data: HashMap<String, Option<LoaderData>>,
}

#[derive(Deserialize)]
struct LoaderData {
    #[serde(rename = "videoInfoRes")]
    video_info: Option<VideoInfoResponse>,
}

#[derive(Deserialize)]
struct VideoInfoResponse {
    status_code: i64,
    #[serde(default)]
    status_msg: String,
    #[serde(default)]
    item_list: Vec<AwemeItem>,
}

#[derive(Deserialize)]
struct AwemeItem {
    aweme_id: String,
    #[serde(default)]
    desc: String,
    author: Author,
    video: Video,
}

#[derive(Deserialize)]
struct Author {
    #[serde(default)]
    nickname: String,
}

#[derive(Deserialize)]
struct Video {
    play_addr: PlayAddress,
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Deserialize)]
struct PlayAddress {
    #[serde(default)]
    url_list: Vec<String>,
}

fn video_id_from_url(input: &str) -> Result<Option<String>> {
    let url = Url::parse(input.trim()).context("抖音链接格式不正确")?;
    let host = url.host_str().context("抖音链接缺少域名")?;
    if !is_douyin_host(host) {
        bail!("不支持的抖音链接域名：{host}");
    }
    let regex = Regex::new(r"/(?:video|share/video)/(\d{10,})").expect("static regex");
    Ok(regex
        .captures(url.path())
        .and_then(|captures| captures.get(1))
        .map(|id| id.as_str().to_owned()))
}

fn is_douyin_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("douyin.com")
        || host.to_ascii_lowercase().ends_with(".douyin.com")
        || host.eq_ignore_ascii_case("iesdouyin.com")
        || host.to_ascii_lowercase().ends_with(".iesdouyin.com")
}

fn parse_router_data(html: &str, expected_id: &str) -> Result<DouyinVideo> {
    let regex = Regex::new(r#"(?s)window\._ROUTER_DATA\s*=\s*(\{.*?\})\s*</script>"#)
        .expect("static regex");
    let json = regex
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str())
        .context("抖音分享页没有包含视频数据，页面结构可能已变化")?;
    let router: RouterData = serde_json::from_str(json).context("解析抖音视频数据失败")?;
    let response = router
        .loader_data
        .into_values()
        .flatten()
        .find_map(|loader| loader.video_info)
        .context("抖音分享页没有返回作品详情")?;
    if response.status_code != 0 {
        bail!(
            "获取抖音作品详情失败（{}）：{}",
            response.status_code,
            if response.status_msg.is_empty() {
                "未知错误"
            } else {
                &response.status_msg
            }
        );
    }
    let item = response
        .item_list
        .into_iter()
        .find(|item| item.aweme_id == expected_id)
        .context("抖音分享页没有返回目标视频")?;
    if item.video.play_addr.url_list.is_empty() {
        bail!("抖音作品没有可用的 MP4 播放地址");
    }
    Ok(DouyinVideo {
        id: item.aweme_id,
        description: nonempty(item.desc, "未命名抖音视频"),
        author: nonempty(item.author.nickname, "未知作者"),
        duration_ms: item.video.duration,
        width: item.video.width,
        height: item.video.height,
        play_urls: item.video.play_addr.url_list,
    })
}

fn download_candidates(play_urls: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for url in play_urls.iter().filter_map(|url| without_watermark(url)) {
        if seen.insert(url.clone()) {
            urls.push(url);
        }
    }
    for url in play_urls {
        if seen.insert(url.clone()) {
            urls.push(url.clone());
        }
    }
    urls
}

fn without_watermark(value: &str) -> Option<String> {
    let mut url = Url::parse(value).ok()?;
    if !url.path().contains("/playwm/") {
        return None;
    }
    url.set_path(&url.path().replace("/playwm/", "/play/"));
    let query = url
        .query_pairs()
        .filter(|(key, _)| key != "logo_name")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    url.query_pairs_mut().extend_pairs(query);
    Some(url.into())
}

fn output_base(info: &DouyinVideo) -> String {
    let author = sanitize_filename(&info.author);
    let description = sanitize_filename(&info.description)
        .chars()
        .take(100)
        .collect::<String>();
    sanitize_filename(&format!("{author}-{description}-{}", info.id))
}

fn nonempty(value: String, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

fn format_duration(duration_ms: u64) -> String {
    let seconds = duration_ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::{download_candidates, is_douyin_input, parse_router_data, video_id_from_url};

    #[test]
    fn recognizes_supported_douyin_urls() {
        assert!(is_douyin_input("https://v.douyin.com/abc/"));
        assert!(is_douyin_input(
            "https://www.douyin.com/video/7660839686302207283"
        ));
        assert!(!is_douyin_input(
            "https://example.com/video/7660839686302207283"
        ));
    }

    #[test]
    fn extracts_direct_video_ids() {
        assert_eq!(
            video_id_from_url("https://www.iesdouyin.com/share/video/7660839686302207283/")
                .unwrap()
                .as_deref(),
            Some("7660839686302207283")
        );
        assert_eq!(
            video_id_from_url("https://v.douyin.com/abc/").unwrap(),
            None
        );
    }

    #[test]
    fn parses_ssr_video_data() {
        let html = r#"<script>window._ROUTER_DATA = {"loaderData":{"video_layout":null,"video_(id)/page":{"videoInfoRes":{"status_code":0,"status_msg":"","item_list":[{"aweme_id":"7660839686302207283","desc":"demo","author":{"nickname":"author"},"video":{"play_addr":{"url_list":["https://aweme.snssdk.com/aweme/v1/playwm/?logo_name=x&video_id=abc"]},"duration":123000,"width":1920,"height":1080}}]}}}}</script>"#;
        let info = parse_router_data(html, "7660839686302207283").unwrap();
        assert_eq!(info.description, "demo");
        assert_eq!(info.author, "author");
        assert_eq!(info.duration_ms, 123000);
    }

    #[test]
    fn prefers_no_watermark_url_and_keeps_original_fallback() {
        let original =
            "https://aweme.snssdk.com/aweme/v1/playwm/?line=0&logo_name=aweme&video_id=abc";
        let urls = download_candidates(&[original.to_owned()]);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("/play/?"));
        assert!(!urls[0].contains("logo_name"));
        assert_eq!(urls[1], original);
    }
}
