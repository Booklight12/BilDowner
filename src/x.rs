use std::collections::HashSet;

use anyhow::{Context, Result, bail};
#[cfg(not(windows))]
use indicatif::MultiProgress;
use regex::Regex;
#[cfg(not(windows))]
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, ORIGIN, REFERER};
use serde::Deserialize;
use url::Url;

#[cfg(not(windows))]
use crate::http_download;
use crate::{api::USER_AGENT, cli::DownloadArgs, download::sanitize_filename};

pub fn is_x_input(input: &str) -> bool {
    Url::parse(input.trim()).ok().is_some_and(|url| {
        url.host_str().is_some_and(is_x_host) && status_id_from_path(url.path()).is_some()
    })
}

pub async fn print_info(input: &str) -> Result<()> {
    let client = XClient::new()?;
    let info = client.video_info(input).await?;
    println!(
        "X 帖子 ID：{}\n作者：@{}\n帖子文本：{}\n时长：{}\n可用 MP4：{}",
        info.id,
        info.author,
        nonempty_text(&info.text),
        format_duration(info.duration_ms),
        format_variants(&info.variants)
    );
    Ok(())
}

pub async fn download(args: DownloadArgs) -> Result<()> {
    if !matches!(args.codec, crate::cli::CodecChoice::Auto) {
        println!("提示：X 的直连 MP4 已指定视频编码，已忽略编码选项。");
    }
    if args.ffmpeg.is_some() {
        println!("提示：X 提供已合并的 MP4，不需要 FFmpeg，已忽略 --ffmpeg。");
    }

    let client = XClient::new()?;
    let info = client.video_info(&args.input).await?;
    let selected = select_variant(&info.variants, &args.quality)?;
    tokio::fs::create_dir_all(&args.output_dir)
        .await
        .with_context(|| format!("无法创建输出目录 {}", args.output_dir.display()))?;
    let destination = args
        .output_dir
        .join(format!("{}.mp4", output_base(&info, selected)));

    println!(
        "准备下载 X 视频：@{} / {}\n时长：{}，选择：{}x{}，{}",
        info.author,
        info.id,
        format_duration(info.duration_ms),
        selected.width,
        selected.height,
        format_bitrate(selected.bitrate)
    );
    #[cfg(windows)]
    crate::wininet::download(
        selected.url.clone(),
        destination.clone(),
        "视频",
        args.force,
        USER_AGENT,
    )
    .await?;
    #[cfg(not(windows))]
    {
        let progress = MultiProgress::new();
        http_download::download_urls(
            std::slice::from_ref(&selected.url),
            &destination,
            "视频",
            &progress,
            args.force,
            true,
            |url| client.media_get(url, &info.id),
        )
        .await?;
    }
    println!("视频文件：{}", destination.display());
    Ok(())
}

struct XClient {
    #[cfg(not(windows))]
    client: reqwest::Client,
}

impl XClient {
    fn new() -> Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {})
        }
        #[cfg(not(windows))]
        {
            let client = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .context("无法创建 X HTTP 客户端")?;
            Ok(Self { client })
        }
    }

    async fn video_info(&self, input: &str) -> Result<XVideo> {
        let id = status_id_from_url(input)?;
        let url =
            format!("https://cdn.syndication.twimg.com/tweet-result?id={id}&lang=zh-cn&token=1");
        #[cfg(windows)]
        let json = crate::wininet::get_text(
            url,
            "Accept: application/json, text/plain, */*\r\nAccept-Language: zh-CN,zh;q=0.9,en;q=0.8\r\nReferer: https://x.com/\r\n".to_owned(),
            USER_AGENT,
        )
        .await?;
        #[cfg(not(windows))]
        let json = self
            .client
            .get(url)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
            .header(REFERER, "https://x.com/")
            .send()
            .await
            .context("请求 X 帖子公开数据失败")?
            .error_for_status()
            .context("X 帖子公开数据返回 HTTP 错误")?
            .text()
            .await
            .context("读取 X 帖子公开数据失败")?;
        parse_api_response(&json, &id)
    }

    #[cfg(not(windows))]
    fn media_get(&self, url: &str, id: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header(ACCEPT, "video/mp4,video/*;q=0.9,*/*;q=0.8")
            .header(ORIGIN, "https://x.com")
            .header(REFERER, format!("https://x.com/i/status/{id}"))
    }
}

#[derive(Debug)]
struct XVideo {
    id: String,
    author: String,
    text: String,
    duration_ms: u64,
    variants: Vec<XVariant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct XVariant {
    url: String,
    bitrate: u64,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
struct SyndicationTweet {
    id_str: Option<String>,
    #[serde(default)]
    text: String,
    user: Option<SyndicationUser>,
    #[serde(rename = "mediaDetails", default)]
    media_details: Vec<SyndicationMedia>,
}

#[derive(Deserialize)]
struct SyndicationUser {
    screen_name: String,
}

#[derive(Deserialize)]
struct SyndicationMedia {
    #[serde(rename = "type")]
    media_type: String,
    video_info: Option<SyndicationVideoInfo>,
    original_info: Option<SyndicationOriginalInfo>,
}

#[derive(Deserialize)]
struct SyndicationVideoInfo {
    #[serde(default)]
    duration_millis: u64,
    #[serde(default)]
    variants: Vec<SyndicationVariant>,
}

#[derive(Deserialize)]
struct SyndicationVariant {
    bitrate: Option<u64>,
    content_type: String,
    url: String,
}

#[derive(Deserialize)]
struct SyndicationOriginalInfo {
    width: u32,
    height: u32,
}

fn status_id_from_url(input: &str) -> Result<String> {
    let url = Url::parse(input.trim()).context("X 链接格式不正确")?;
    let host = url.host_str().context("X 链接缺少域名")?;
    if !is_x_host(host) {
        bail!("不支持的 X 链接域名：{host}");
    }
    status_id_from_path(url.path()).context("X 链接中没有找到帖子 ID")
}

fn status_id_from_path(path: &str) -> Option<String> {
    let regex = Regex::new(r"/(?:[^/]+/)?status/(\d+)").expect("static regex");
    regex
        .captures(path)
        .and_then(|captures| captures.get(1))
        .map(|id| id.as_str().to_owned())
}

fn is_x_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "x.com"
        || host.ends_with(".x.com")
        || host == "twitter.com"
        || host.ends_with(".twitter.com")
}

fn parse_api_response(json: &str, expected_id: &str) -> Result<XVideo> {
    let response: SyndicationTweet =
        serde_json::from_str(json).context("解析 X 帖子公开数据失败")?;
    let id = response
        .id_str
        .context("X 没有返回该帖子；帖子可能已删除或受账号/年龄限制")?;
    if id != expected_id {
        bail!("X 返回了其他帖子的数据：期望 {expected_id}，实际 {id}");
    }
    let author = response
        .user
        .map(|user| user.screen_name)
        .unwrap_or_else(|| "unknown".to_owned());
    let media = response
        .media_details
        .into_iter()
        .find(|media| matches!(media.media_type.as_str(), "video" | "animated_gif"))
        .context("X 帖子没有公开的视频")?;
    let original_dimensions = media.original_info.map(|info| (info.width, info.height));
    let video_info = media.video_info.context("X 帖子没有返回视频播放信息")?;
    let dimension_regex = Regex::new(r"/(\d+)x(\d+)/").expect("static regex");

    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    for variant in video_info.variants {
        if variant.content_type != "video/mp4" || !seen.insert(variant.url.clone()) {
            continue;
        }
        let dimensions = dimension_regex
            .captures(&variant.url)
            .and_then(|value| Some((value.get(1)?, value.get(2)?)))
            .and_then(|(width, height)| {
                Some((width.as_str().parse().ok()?, height.as_str().parse().ok()?))
            })
            .or(original_dimensions)
            .context("X 视频地址没有包含分辨率")?;
        variants.push(XVariant {
            url: variant.url,
            bitrate: variant.bitrate.unwrap_or(0),
            width: dimensions.0,
            height: dimensions.1,
        });
    }
    if variants.is_empty() {
        bail!("X 帖子没有公开的 MP4 视频变体");
    }
    variants.sort_by_key(|variant| (variant.height, variant.width, variant.bitrate));

    Ok(XVideo {
        id,
        author,
        text: response.text,
        duration_ms: video_info.duration_millis,
        variants,
    })
}

fn select_variant<'a>(variants: &'a [XVariant], quality: &str) -> Result<&'a XVariant> {
    if quality.trim().eq_ignore_ascii_case("best") {
        return variants
            .iter()
            .max_by_key(|variant| (variant.bitrate, variant.height, variant.width))
            .context("X 帖子没有可下载的视频变体");
    }

    let target = requested_height(quality)?;
    variants
        .iter()
        .filter(|variant| variant.height <= target)
        .max_by_key(|variant| (variant.height, variant.bitrate, variant.width))
        .or_else(|| variants.iter().min_by_key(|variant| variant.height))
        .context("X 帖子没有可下载的视频变体")
}

fn requested_height(value: &str) -> Result<u32> {
    let normalized = value.trim().to_ascii_lowercase();
    let height = match normalized.as_str() {
        "4k" | "2160p" => 2160,
        "2k" | "1440p" => 1440,
        "1080p" | "1080p+" | "1080p60" => 1080,
        "720p" => 720,
        "480p" => 480,
        "360p" => 360,
        "270p" => 270,
        _ => normalized
            .strip_suffix('p')
            .unwrap_or(&normalized)
            .parse::<u32>()
            .with_context(|| format!("不支持的 X 视频清晰度：{value}"))?,
    };
    if height == 0 {
        bail!("X 视频清晰度必须大于 0");
    }
    Ok(height)
}

fn format_variants(variants: &[XVariant]) -> String {
    variants
        .iter()
        .map(|variant| {
            format!(
                "{}x{} ({})",
                variant.width,
                variant.height,
                format_bitrate(variant.bitrate)
            )
        })
        .collect::<Vec<_>>()
        .join("、")
}

fn format_bitrate(bitrate: u64) -> String {
    if bitrate >= 1_000_000 {
        format!("{:.2} Mbps", bitrate as f64 / 1_000_000.0)
    } else {
        format!("{} Kbps", bitrate / 1000)
    }
}

fn format_duration(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1000;
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
}

fn nonempty_text(value: &str) -> &str {
    if value.trim().is_empty() {
        "（仅媒体）"
    } else {
        value
    }
}

fn output_base(info: &XVideo, variant: &XVariant) -> String {
    sanitize_filename(&format!(
        "X-@{}-{}-{}p",
        info.author, info.id, variant.height
    ))
}

#[cfg(test)]
mod tests {
    use super::{XVariant, is_x_input, parse_api_response, select_variant, status_id_from_url};

    const RESPONSE: &str = r#"{
        "id_str":"2077521842080817296",
        "text":"demo\npost",
        "user":{"screen_name":"Kimi_Moonshot"},
        "mediaDetails":[{
            "type":"video",
            "original_info":{"width":1280,"height":720},
            "video_info":{"duration_millis":36041,"variants":[
                {"content_type":"application/x-mpegURL","url":"https://video.twimg.com/playlist.m3u8"},
                {"bitrate":256000,"content_type":"video/mp4","url":"https://video.twimg.com/amplify_video/123/vid/avc1/480x270/low.mp4"},
                {"bitrate":2176000,"content_type":"video/mp4","url":"https://video.twimg.com/amplify_video/123/vid/avc1/1280x720/high.mp4"}
            ]}
        },{
            "type":"video",
            "video_info":{"duration_millis":1000,"variants":[
                {"bitrate":9999999,"content_type":"video/mp4","url":"https://video.twimg.com/amplify_video/456/vid/avc1/1920x1080/other.mp4"}
            ]}
        }]
    }"#;

    #[test]
    fn recognizes_x_and_twitter_status_urls() {
        assert!(is_x_input("https://x.com/user/status/2077521842080817296"));
        assert!(is_x_input(
            "https://mobile.twitter.com/user/status/2077521842080817296/video/1"
        ));
        assert!(!is_x_input(
            "https://example.com/user/status/2077521842080817296"
        ));
        assert!(!is_x_input("https://x.com/user"));
    }

    #[test]
    fn extracts_status_id() {
        assert_eq!(
            status_id_from_url("https://x.com/i/status/2077521842080817296?lang=zh").unwrap(),
            "2077521842080817296"
        );
    }

    #[test]
    fn parses_first_video_and_its_variants() {
        let info = parse_api_response(RESPONSE, "2077521842080817296").unwrap();
        assert_eq!(info.author, "Kimi_Moonshot");
        assert_eq!(info.text, "demo\npost");
        assert_eq!(info.duration_ms, 36041);
        assert_eq!(info.variants.len(), 2);
        assert_eq!(info.variants[1].height, 720);
    }

    #[test]
    fn selects_best_or_highest_not_above_requested_height() {
        let variants = vec![
            XVariant {
                url: "low".to_owned(),
                bitrate: 256_000,
                width: 480,
                height: 270,
            },
            XVariant {
                url: "high".to_owned(),
                bitrate: 2_176_000,
                width: 1280,
                height: 720,
            },
        ];
        assert_eq!(select_variant(&variants, "best").unwrap().height, 720);
        assert_eq!(select_variant(&variants, "480p").unwrap().height, 270);
        assert_eq!(select_variant(&variants, "1080p").unwrap().height, 720);
    }
}
