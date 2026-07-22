use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, COOKIE, ORIGIN, REFERER};
use serde::Deserialize;
use url::Url;

use crate::cli::CodecChoice;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36 Edg/138.0.0.0";

const VIEW_API: &str = "https://api.bilibili.com/x/web-interface/view";
const NAV_API: &str = "https://api.bilibili.com/x/web-interface/nav";
const PLAY_API: &str = "https://api.bilibili.com/x/player/wbi/playurl";
const SEASON_API: &str = "https://api.bilibili.com/pgc/view/web/season";
const PGC_PLAY_API: &str = "https://api.bilibili.com/pgc/player/web/playurl";
const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

pub struct BiliClient {
    client: reqwest::Client,
    cookie: Option<String>,
}

impl BiliClient {
    pub fn new(cookie: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("无法创建 HTTP 客户端")?;
        Ok(Self { client, cookie })
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self
            .client
            .get(url)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
            .header(ORIGIN, "https://www.bilibili.com")
            .header(REFERER, "https://www.bilibili.com/");
        if let Some(cookie) = &self.cookie {
            request.header(COOKIE, cookie)
        } else {
            request
        }
    }

    pub fn media_get(&self, url: &str, referer: &str) -> reqwest::RequestBuilder {
        let request = self
            .client
            .get(url)
            .header(REFERER, referer)
            .header(ORIGIN, "https://www.bilibili.com")
            .header(ACCEPT, "*/*");
        if let Some(cookie) = &self.cookie {
            request.header(COOKIE, cookie)
        } else {
            request
        }
    }

    pub async fn video_info(&self, input: &str) -> Result<VideoInfo> {
        let id = parse_video_id(input)?;
        match id {
            VideoId::Bvid(value) => self.normal_video_info("bvid", &value).await,
            VideoId::Aid(value) => self.normal_video_info("aid", &value).await,
            VideoId::Episode(ep_id) => self.season_video_info(Some(ep_id), None).await,
            VideoId::Season(season_id) => self.season_video_info(None, Some(season_id)).await,
        }
    }

    async fn normal_video_info(&self, key: &str, value: &str) -> Result<VideoInfo> {
        self.get(VIEW_API)
            .query(&[(key, value)])
            .send()
            .await
            .context("请求视频信息失败")?
            .error_for_status()
            .context("视频信息接口返回 HTTP 错误")?
            .json::<ApiEnvelope<VideoInfo>>()
            .await
            .context("解析视频信息失败")?
            .into_data("获取视频信息")
    }

    async fn season_video_info(
        &self,
        ep_id: Option<u64>,
        season_id: Option<u64>,
    ) -> Result<VideoInfo> {
        let request = if let Some(ep_id) = ep_id {
            self.get(SEASON_API).query(&[("ep_id", ep_id)])
        } else {
            self.get(SEASON_API)
                .query(&[("season_id", season_id.context("缺少 season_id")?)])
        };
        let response = request
            .send()
            .await
            .context("请求番剧/影视信息失败")?
            .error_for_status()
            .context("番剧/影视信息接口返回 HTTP 错误")?
            .json::<PgcEnvelope<SeasonInfo>>()
            .await
            .context("解析番剧/影视信息失败")?;
        let season = response.into_result("获取番剧/影视信息")?;
        let episodes = if let Some(ep_id) = ep_id {
            season
                .episodes
                .into_iter()
                .filter(|episode| episode.id == ep_id)
                .collect::<Vec<_>>()
        } else {
            season.episodes
        };
        if episodes.is_empty() {
            bail!("番剧/影视响应中没有可下载的剧集");
        }
        let first = &episodes[0];
        Ok(VideoInfo {
            bvid: first.bvid.clone(),
            aid: first.aid,
            title: season.title,
            pages: episodes
                .into_iter()
                .enumerate()
                .map(|(index, episode)| VideoPage {
                    cid: episode.cid,
                    page: index + 1,
                    part: episode.display_title(),
                    duration: episode.duration / 1000,
                    bvid: Some(episode.bvid),
                    ep_id: Some(episode.id),
                })
                .collect(),
        })
    }

    pub async fn nav(&self) -> Result<NavData> {
        let response = self
            .get(NAV_API)
            .send()
            .await
            .context("请求 Bilibili 导航信息失败")?
            .error_for_status()
            .context("导航接口返回 HTTP 错误")?
            .json::<ApiEnvelope<NavData>>()
            .await
            .context("解析 Bilibili 导航信息失败")?;
        response
            .data
            .context("导航接口没有返回 data（WBI 密钥不可用）")
    }

    pub async fn play_info(&self, bvid: &str, cid: u64, qn: u32) -> Result<PlayData> {
        let nav = self.nav().await?;
        let mixin_key = make_mixin_key(&nav.wbi_img)?;
        let mut params = BTreeMap::from([
            ("bvid".to_owned(), bvid.to_owned()),
            ("cid".to_owned(), cid.to_string()),
            ("fnval".to_owned(), "4048".to_owned()),
            ("fnver".to_owned(), "0".to_owned()),
            ("fourk".to_owned(), "1".to_owned()),
            ("qn".to_owned(), qn.to_string()),
        ]);
        sign_wbi(&mut params, &mixin_key)?;

        self.get(PLAY_API)
            .query(&params)
            .header(REFERER, format!("https://www.bilibili.com/video/{bvid}"))
            .send()
            .await
            .context("请求播放地址失败")?
            .error_for_status()
            .context("播放地址接口返回 HTTP 错误")?
            .json::<ApiEnvelope<PlayData>>()
            .await
            .context("解析播放地址失败")?
            .into_data("获取播放地址")
    }

    pub async fn play_info_for_page(
        &self,
        bvid: &str,
        cid: u64,
        ep_id: Option<u64>,
        qn: u32,
    ) -> Result<PlayData> {
        if let Some(ep_id) = ep_id {
            self.pgc_play_info(ep_id, qn).await
        } else {
            self.play_info(bvid, cid, qn).await
        }
    }

    async fn pgc_play_info(&self, ep_id: u64, qn: u32) -> Result<PlayData> {
        let play = self
            .get(PGC_PLAY_API)
            .query(&[
                ("ep_id", ep_id.to_string()),
                ("qn", qn.to_string()),
                ("fnver", "0".to_owned()),
                ("fnval", "4048".to_owned()),
                ("fourk", "1".to_owned()),
            ])
            .header(
                REFERER,
                format!("https://www.bilibili.com/bangumi/play/ep{ep_id}"),
            )
            .send()
            .await
            .context("请求番剧/影视播放地址失败")?
            .error_for_status()
            .context("番剧/影视播放地址接口返回 HTTP 错误")?
            .json::<PgcEnvelope<PlayData>>()
            .await
            .context("解析番剧/影视播放地址失败")?
            .into_result("获取番剧/影视播放地址")?;
        if play.code.unwrap_or(0) != 0 {
            bail!(
                "获取番剧/影视播放地址失败（{}）：{}",
                play.code.unwrap_or_default(),
                play.message.as_deref().unwrap_or("未知错误")
            );
        }
        if play.is_drm {
            bail!("该剧集返回了 DRM 保护流，本工具不会绕过 DRM");
        }
        Ok(play)
    }
}

pub async fn print_info(client: &BiliClient, input: &str, page: Option<&str>) -> Result<()> {
    let info = client.video_info(input).await?;
    println!("{}\nBV: {}  AV: {}", info.title, info.bvid, info.aid);
    println!("分 P：");
    for item in &info.pages {
        println!("  P{}  {:>6}s  {}", item.page, item.duration, item.part);
    }

    let selection = info.resolve_pages(input, page)?;
    print_page_selection_notice(&info, page, &selection, "检查");
    for page_number in selection.pages {
        let selected = info.page(page_number)?;
        let play = client
            .play_info_for_page(info.page_bvid(selected), selected.cid, selected.ep_id, 127)
            .await?;
        println!("\nP{} 可用清晰度：", selected.page);
        for quality in play.available_qualities() {
            let codecs = play
                .dash
                .as_ref()
                .map(|dash| {
                    dash.video
                        .iter()
                        .filter(|stream| stream.id == quality)
                        .map(|stream| codec_name(stream.codecid, &stream.codecs))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            println!(
                "  {:>3}  {:<14} {}",
                quality,
                play.quality_description(quality),
                codecs
            );
        }
    }
    if client.cookie.is_none() {
        println!("\n当前未登录；登录后通常可获得更高清晰度。");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ApiEnvelope<T> {
    pub code: i64,
    #[serde(default)]
    pub message: String,
    pub data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct PgcEnvelope<T> {
    code: i64,
    #[serde(default)]
    message: String,
    result: Option<T>,
}

impl<T> PgcEnvelope<T> {
    fn into_result(self, operation: &str) -> Result<T> {
        if self.code != 0 {
            bail!("{operation}失败（{}）：{}", self.code, self.message);
        }
        self.result
            .with_context(|| format!("{operation}成功，但响应中没有 result"))
    }
}

impl<T> ApiEnvelope<T> {
    pub fn into_data(self, operation: &str) -> Result<T> {
        if self.code != 0 {
            bail!("{operation}失败（{}）：{}", self.code, self.message);
        }
        self.data
            .with_context(|| format!("{operation}成功，但响应中没有 data"))
    }
}

#[derive(Debug, Deserialize)]
pub struct VideoInfo {
    pub bvid: String,
    pub aid: u64,
    pub title: String,
    #[serde(default)]
    pub pages: Vec<VideoPage>,
}

impl VideoInfo {
    pub fn page(&self, page: usize) -> Result<&VideoPage> {
        if page == 0 {
            bail!("分 P 序号必须从 1 开始");
        }
        self.pages
            .get(page - 1)
            .with_context(|| format!("P{page} 不存在；该视频共有 {} 个分 P", self.pages.len()))
    }

    pub fn page_bvid<'a>(&'a self, page: &'a VideoPage) -> &'a str {
        page.bvid.as_deref().unwrap_or(&self.bvid)
    }

    pub fn resolve_pages(&self, input: &str, selection: Option<&str>) -> Result<PageSelection> {
        if self.pages.is_empty() {
            bail!("视频信息中没有可下载的分 P");
        }
        if self.pages.len() == 1 {
            return Ok(PageSelection {
                pages: vec![1],
                source: PageSelectionSource::SinglePage,
            });
        }

        if let Some(selection) = selection {
            return Ok(PageSelection {
                pages: parse_page_selection(selection, self.pages.len())?,
                source: PageSelectionSource::Explicit,
            });
        }
        if let Some(page) = page_from_url(input)? {
            self.page(page)?;
            return Ok(PageSelection {
                pages: vec![page],
                source: PageSelectionSource::Url,
            });
        }
        Ok(PageSelection {
            pages: vec![1],
            source: PageSelectionSource::Default,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSelectionSource {
    Explicit,
    Url,
    Default,
    SinglePage,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PageSelection {
    pub pages: Vec<usize>,
    pub source: PageSelectionSource,
}

pub fn print_page_selection_notice(
    info: &VideoInfo,
    explicit: Option<&str>,
    selection: &PageSelection,
    action: &str,
) {
    match selection.source {
        PageSelectionSource::SinglePage if explicit.is_some() => eprintln!(
            "警告：该视频没有分 P；已忽略 --page `{}`，将{}唯一的视频。",
            explicit.unwrap_or_default(),
            action
        ),
        PageSelectionSource::Default => eprintln!(
            "提示：检测到 {} 个分 P，未指定 --page，默认{} P1。",
            info.pages.len(),
            action
        ),
        PageSelectionSource::Explicit | PageSelectionSource::Url => println!(
            "已选择分 P：{}",
            selection
                .pages
                .iter()
                .map(|page| format!("P{page}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        PageSelectionSource::SinglePage => {}
    }
}

#[derive(Debug, Deserialize)]
pub struct VideoPage {
    pub cid: u64,
    pub page: usize,
    pub part: String,
    pub duration: u64,
    #[serde(default)]
    pub bvid: Option<String>,
    #[serde(default)]
    pub ep_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SeasonInfo {
    title: String,
    #[serde(default)]
    episodes: Vec<SeasonEpisode>,
}

#[derive(Debug, Deserialize)]
struct SeasonEpisode {
    id: u64,
    aid: u64,
    bvid: String,
    cid: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    long_title: String,
    #[serde(default)]
    duration: u64,
}

impl SeasonEpisode {
    fn display_title(&self) -> String {
        match (self.title.trim(), self.long_title.trim()) {
            ("", "") => format!("ep{}", self.id),
            ("", long) => long.to_owned(),
            (title, "") => title.to_owned(),
            (title, long) => format!("{title} {long}"),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct NavData {
    #[serde(rename = "isLogin")]
    pub is_login: bool,
    pub uname: Option<String>,
    pub mid: Option<u64>,
    pub wbi_img: WbiImages,
}

#[derive(Debug, Deserialize)]
pub struct WbiImages {
    img_url: String,
    sub_url: String,
}

#[derive(Debug, Deserialize)]
pub struct PlayData {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    is_drm: bool,
    #[serde(default)]
    pub accept_quality: Vec<u32>,
    #[serde(default)]
    pub accept_description: Vec<String>,
    #[serde(default)]
    pub support_formats: Vec<SupportFormat>,
    pub dash: Option<Dash>,
}

impl PlayData {
    pub fn available_qualities(&self) -> Vec<u32> {
        let mut qualities = self
            .dash
            .as_ref()
            .map(|dash| {
                dash.video
                    .iter()
                    .map(|stream| stream.id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| self.accept_quality.clone());
        qualities.sort_unstable_by(|a, b| b.cmp(a));
        qualities.dedup();
        qualities
    }

    pub fn quality_description(&self, id: u32) -> String {
        if let Some(format) = self.support_formats.iter().find(|item| item.quality == id) {
            return format
                .new_description
                .as_deref()
                .or(format.display_desc.as_deref())
                .unwrap_or_else(|| quality_name(id))
                .to_owned();
        }
        if let Some(index) = self
            .accept_quality
            .iter()
            .position(|quality| *quality == id)
            && let Some(description) = self.accept_description.get(index)
        {
            return description.clone();
        }
        quality_name(id).to_owned()
    }

    pub fn select_streams(
        &self,
        requested_quality: &str,
        codec: CodecChoice,
    ) -> Result<(&DashStream, &DashStream, String)> {
        let dash = self
            .dash
            .as_ref()
            .context("接口没有返回 DASH 音视频流，无法进行音视频分离下载")?;
        let available = self.available_qualities();
        let quality = parse_quality(requested_quality, &available)?;
        let candidates = dash
            .video
            .iter()
            .filter(|stream| stream.id == quality)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            bail!(
                "清晰度 {} 当前不可用；可用值：{}",
                requested_quality,
                available
                    .iter()
                    .map(|id| format!("{}({id})", self.quality_description(*id)))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let video = if let Some(codecid) = codec.codecid() {
            candidates
                .iter()
                .copied()
                .find(|stream| stream.codecid == codecid)
                .with_context(|| {
                    let available = candidates
                        .iter()
                        .map(|stream| codec_name(stream.codecid, &stream.codecs))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("该清晰度没有所选编码；可用编码：{available}")
                })?
        } else {
            [7, 12, 13]
                .into_iter()
                .find_map(|codecid| {
                    candidates
                        .iter()
                        .copied()
                        .find(|stream| stream.codecid == codecid)
                })
                .unwrap_or(candidates[0])
        };
        let audio = dash
            .audio
            .iter()
            .max_by_key(|stream| stream.bandwidth)
            .context("DASH 响应中没有标准音频流")?;
        Ok((video, audio, self.quality_description(quality)))
    }
}

#[derive(Debug, Deserialize)]
pub struct SupportFormat {
    pub quality: u32,
    pub new_description: Option<String>,
    pub display_desc: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Dash {
    #[serde(default)]
    pub video: Vec<DashStream>,
    #[serde(default)]
    pub audio: Vec<DashStream>,
}

#[derive(Debug, Deserialize)]
pub struct DashStream {
    pub id: u32,
    pub base_url: String,
    #[serde(default)]
    pub backup_url: Vec<String>,
    #[serde(default)]
    pub bandwidth: u64,
    #[serde(default)]
    pub codecs: String,
    #[serde(default)]
    pub codecid: u32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub frame_rate: String,
}

impl DashStream {
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.base_url.as_str()).chain(self.backup_url.iter().map(String::as_str))
    }
}

enum VideoId {
    Bvid(String),
    Aid(String),
    Episode(u64),
    Season(u64),
}

fn page_from_url(input: &str) -> Result<Option<usize>> {
    let Ok(url) = Url::parse(input.trim()) else {
        return Ok(None);
    };
    let Some(value) = url
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("p"))
        .map(|(_, value)| value.into_owned())
    else {
        return Ok(None);
    };
    let page = value
        .parse::<usize>()
        .with_context(|| format!("链接中的分 P 参数 `p={value}` 不合法"))?;
    if page == 0 {
        bail!("链接中的分 P 参数必须从 1 开始");
    }
    Ok(Some(page))
}

fn parse_page_selection(value: &str, page_count: usize) -> Result<Vec<usize>> {
    let value = value.trim();
    if value.is_empty() {
        bail!("--page 不能为空");
    }

    let mut pages = Vec::new();
    let mut seen = BTreeSet::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            bail!("--page `{value}` 中存在空的分 P 序号");
        }
        if let Some((start, end)) = item.split_once('-') {
            if end.contains('-') {
                bail!("无法识别分 P 范围 `{item}`");
            }
            let start = parse_page_number(start, page_count)?;
            let end = parse_page_number(end, page_count)?;
            if start > end {
                bail!("分 P 范围 `{item}` 的起点不能大于终点");
            }
            for page in start..=end {
                if seen.insert(page) {
                    pages.push(page);
                }
            }
        } else {
            let page = parse_page_number(item, page_count)?;
            if seen.insert(page) {
                pages.push(page);
            }
        }
    }
    Ok(pages)
}

fn parse_page_number(value: &str, page_count: usize) -> Result<usize> {
    let value = value.trim();
    let page = value
        .parse::<usize>()
        .with_context(|| format!("分 P 序号 `{value}` 不合法"))?;
    if page == 0 {
        bail!("分 P 序号必须从 1 开始");
    }
    if page > page_count {
        bail!("P{page} 不存在；该视频共有 {page_count} 个分 P");
    }
    Ok(page)
}

fn parse_video_id(input: &str) -> Result<VideoId> {
    let re = Regex::new(r"(?i)(BV[0-9A-Za-z]+|av[0-9]+|ep[0-9]+|ss[0-9]+)").expect("static regex");
    let value = re
        .find(input.trim())
        .map(|value| value.as_str())
        .context("无法从输入中找到 BV、AV、ep 或 ss 号")?;
    if value[..2].eq_ignore_ascii_case("bv") {
        Ok(VideoId::Bvid(value.to_owned()))
    } else if value[..2].eq_ignore_ascii_case("av") {
        Ok(VideoId::Aid(value[2..].to_owned()))
    } else if value[..2].eq_ignore_ascii_case("ep") {
        Ok(VideoId::Episode(value[2..].parse().context("ep 号不合法")?))
    } else {
        Ok(VideoId::Season(value[2..].parse().context("ss 号不合法")?))
    }
}

fn make_mixin_key(images: &WbiImages) -> Result<String> {
    fn file_stem(url: &str) -> Result<String> {
        let url = Url::parse(url).context("WBI 图片 URL 不合法")?;
        let file = url
            .path_segments()
            .and_then(Iterator::last)
            .context("WBI 图片 URL 没有文件名")?;
        Ok(file.split('.').next().unwrap_or(file).to_owned())
    }

    let source = format!(
        "{}{}",
        file_stem(&images.img_url)?,
        file_stem(&images.sub_url)?
    );
    let chars = source.chars().collect::<Vec<_>>();
    if chars.len() < 64 {
        bail!("WBI 密钥长度异常");
    }
    Ok(MIXIN_KEY_ENC_TAB
        .iter()
        .filter_map(|index| chars.get(*index))
        .take(32)
        .collect())
}

fn sign_wbi(params: &mut BTreeMap<String, String>, mixin_key: &str) -> Result<()> {
    let wts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 Unix epoch")?
        .as_secs();
    params.insert("wts".to_owned(), wts.to_string());
    for value in params.values_mut() {
        value.retain(|character| !"!'()*".contains(character));
    }
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(
        params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    let query = serializer.finish();
    let digest = format!("{:x}", md5::compute(format!("{query}{mixin_key}")));
    params.insert("w_rid".to_owned(), digest);
    Ok(())
}

pub fn parse_quality(value: &str, available: &[u32]) -> Result<u32> {
    let normalized = value.trim().to_ascii_lowercase().replace([' ', '_'], "");
    if normalized == "best" || normalized == "最高" {
        return available.first().copied().context("没有可用清晰度");
    }
    let id = match normalized.as_str() {
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
        _ => normalized
            .parse::<u32>()
            .with_context(|| format!("无法识别清晰度 `{value}`"))?,
    };
    if available.contains(&id) {
        Ok(id)
    } else {
        bail!(
            "请求的清晰度 `{value}` 当前不可用；可用 qn：{}",
            available
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

pub fn quality_name(id: u32) -> &'static str {
    match id {
        6 => "240P 极速",
        16 => "360P 流畅",
        32 => "480P 清晰",
        64 => "720P 高清",
        74 => "720P60",
        80 => "1080P 高清",
        112 => "1080P 高码率",
        116 => "1080P60",
        120 => "4K 超高清",
        125 => "HDR 真彩",
        126 => "杜比视界",
        127 => "8K 超高清",
        _ => "未知清晰度",
    }
}

pub fn codec_name(codecid: u32, codecs: &str) -> String {
    match codecid {
        7 => "AVC/H.264".to_owned(),
        12 => "HEVC/H.265".to_owned(),
        13 => "AV1".to_owned(),
        _ if !codecs.is_empty() => codecs.to_owned(),
        _ => format!("codecid={codecid}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PageSelectionSource, VideoInfo, VideoPage, parse_page_selection, parse_quality,
        parse_video_id,
    };

    fn video_info(page_count: usize) -> VideoInfo {
        VideoInfo {
            bvid: "BV1xx411c7mD".to_owned(),
            aid: 1,
            title: "demo".to_owned(),
            pages: (1..=page_count)
                .map(|page| VideoPage {
                    cid: page as u64,
                    page,
                    part: format!("part {page}"),
                    duration: 60,
                    bvid: None,
                    ep_id: None,
                })
                .collect(),
        }
    }

    #[test]
    fn parses_bv_from_url() {
        assert!(matches!(
            parse_video_id("https://www.bilibili.com/video/BV1xx411c7mD?p=1").unwrap(),
            super::VideoId::Bvid(value) if value == "BV1xx411c7mD"
        ));
    }

    #[test]
    fn parses_episode_and_season_urls() {
        assert!(matches!(
            parse_video_id("https://www.bilibili.com/bangumi/play/ep693247").unwrap(),
            super::VideoId::Episode(693247)
        ));
        assert!(matches!(
            parse_video_id("ss43164").unwrap(),
            super::VideoId::Season(43164)
        ));
    }

    #[test]
    fn parses_page_numbers_ranges_and_lists() {
        assert_eq!(parse_page_selection("7", 10).unwrap(), vec![7]);
        assert_eq!(
            parse_page_selection("1-7", 10).unwrap(),
            (1..=7).collect::<Vec<_>>()
        );
        assert_eq!(parse_page_selection("5,7", 10).unwrap(), vec![5, 7]);
        assert_eq!(
            parse_page_selection("1-3, 5, 3, 7", 10).unwrap(),
            vec![1, 2, 3, 5, 7]
        );
    }

    #[test]
    fn rejects_invalid_page_selections() {
        assert!(parse_page_selection("", 10).is_err());
        assert!(parse_page_selection("0", 10).is_err());
        assert!(parse_page_selection("7-1", 10).is_err());
        assert!(parse_page_selection("1,,2", 10).is_err());
        assert!(parse_page_selection("11", 10).is_err());
    }

    #[test]
    fn resolves_explicit_url_and_default_page_sources() {
        let info = video_info(8);
        let input = "https://www.bilibili.com/video/BV1pVkrYeEer?spm_id_from=333.788.videopod.sections&vd_source=test&p=7";

        let explicit = info.resolve_pages(input, Some("5,7")).unwrap();
        assert_eq!(explicit.pages, vec![5, 7]);
        assert_eq!(explicit.source, PageSelectionSource::Explicit);

        let from_url = info.resolve_pages(input, None).unwrap();
        assert_eq!(from_url.pages, vec![7]);
        assert_eq!(from_url.source, PageSelectionSource::Url);

        let default = info.resolve_pages("BV1xx411c7mD", None).unwrap();
        assert_eq!(default.pages, vec![1]);
        assert_eq!(default.source, PageSelectionSource::Default);
    }

    #[test]
    fn ignores_page_selection_for_single_page_video() {
        let selection = video_info(1)
            .resolve_pages("https://www.bilibili.com/video/BV1xx411c7mD?p=7", Some("7"))
            .unwrap();
        assert_eq!(selection.pages, vec![1]);
        assert_eq!(selection.source, PageSelectionSource::SinglePage);
    }

    #[test]
    fn parses_quality_aliases() {
        assert_eq!(parse_quality("1080p+", &[120, 112, 80]).unwrap(), 112);
        assert_eq!(parse_quality("best", &[120, 112, 80]).unwrap(), 120);
        assert!(parse_quality("8k", &[80, 64]).is_err());
    }
}
