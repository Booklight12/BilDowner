use std::{
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use qrcode::{QrCode, render::unicode};
use reqwest::header::SET_COOKIE;
use serde::Deserialize;

use crate::api::{ApiEnvelope, BiliClient};

const QR_GENERATE: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/generate";
const QR_POLL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/poll";
const MAGIC: &[u8] = b"BDCK1";

pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    pub fn new() -> Result<Self> {
        let path = if let Some(path) = env::var_os("BILDOWNER_COOKIE_FILE") {
            PathBuf::from(path)
        } else {
            config_dir()?.join("BilDowner").join("cookie.dat")
        };
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self, cookie: &str) -> Result<()> {
        let cookie = normalize_cookie(cookie)?;
        let parent = self.path.parent().context("Cookie 文件路径没有父目录")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建配置目录 {}", parent.display()))?;

        let mut bytes = MAGIC.to_vec();
        bytes.extend(protect(cookie.as_bytes())?);
        let temp = self.path.with_extension("tmp");
        fs::write(&temp, bytes)
            .with_context(|| format!("无法写入临时 Cookie 文件 {}", temp.display()))?;
        if self.path.exists() {
            fs::remove_file(&self.path)
                .with_context(|| format!("无法替换 Cookie 文件 {}", self.path.display()))?;
        }
        fs::rename(&temp, &self.path)
            .with_context(|| format!("无法保存 Cookie 文件 {}", self.path.display()))?;
        Ok(())
    }

    pub fn load(&self) -> Result<Option<String>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("无法读取 Cookie 文件 {}", self.path.display()))?;
        let encrypted = bytes
            .strip_prefix(MAGIC)
            .context("Cookie 文件格式不正确；可以执行 `bildowner auth clear` 后重新登录")?;
        let clear = unprotect(encrypted)?;
        String::from_utf8(clear)
            .context("解密后的 Cookie 不是 UTF-8")
            .map(Some)
    }

    pub fn clear(&self) -> Result<bool> {
        if self.path.exists() {
            fs::remove_file(&self.path)
                .with_context(|| format!("无法删除 Cookie 文件 {}", self.path.display()))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub fn set_cookie(
    store: &AuthStore,
    cookie: Option<String>,
    cookie_file: Option<PathBuf>,
) -> Result<()> {
    let cookie = if let Some(cookie) = cookie {
        cookie
    } else if let Some(path) = cookie_file {
        fs::read_to_string(&path)
            .with_context(|| format!("无法读取 Cookie 文件 {}", path.display()))?
    } else {
        eprintln!(
            "请粘贴 Cookie，然后按 Ctrl+Z、回车结束输入（PowerShell 也可用 Get-Clipboard | ...）："
        );
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .context("读取标准输入失败")?;
        input
    };
    store.save(&cookie)?;
    println!(
        "Cookie 已保存到 {}（Windows DPAPI 当前用户加密）",
        store.path().display()
    );
    Ok(())
}

pub fn clear(store: &AuthStore) -> Result<()> {
    if store.clear()? {
        println!("已删除 {}", store.path().display());
    } else {
        println!("尚未保存 Cookie");
    }
    Ok(())
}

pub async fn status(store: &AuthStore) -> Result<()> {
    let Some(cookie) = store.load()? else {
        println!("未保存 Cookie。请执行 `bildowner auth qr`。");
        return Ok(());
    };
    let client = BiliClient::new(Some(cookie))?;
    let nav = client.nav().await?;
    if nav.is_login {
        println!(
            "已登录：{} (mid={})\nCookie：{}",
            nav.uname.as_deref().unwrap_or("未知用户"),
            nav.mid.unwrap_or_default(),
            store.path().display()
        );
    } else {
        println!("Cookie 已保存，但登录已失效。请重新执行 `bildowner auth qr`。");
    }
    Ok(())
}

pub async fn qr_login(store: &AuthStore) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(crate::api::USER_AGENT)
        .build()
        .context("无法创建 HTTP 客户端")?;

    let generated: ApiEnvelope<QrGenerate> = client
        .get(QR_GENERATE)
        .send()
        .await
        .context("请求登录二维码失败")?
        .error_for_status()
        .context("登录二维码接口返回 HTTP 错误")?
        .json()
        .await
        .context("解析登录二维码失败")?;
    let generated = generated.into_data("生成登录二维码")?;
    let code = QrCode::new(generated.url.as_bytes()).context("生成终端二维码失败")?;
    println!(
        "请用哔哩哔哩手机客户端扫码确认登录：\n{}",
        code.render::<unicode::Dense1x2>().quiet_zone(true).build()
    );

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let response = client
            .get(QR_POLL)
            .query(&[("qrcode_key", generated.qrcode_key.as_str())])
            .send()
            .await
            .context("轮询二维码状态失败")?
            .error_for_status()
            .context("二维码状态接口返回 HTTP 错误")?;
        let cookies = extract_set_cookies(response.headers());
        let polled: ApiEnvelope<QrPoll> = response.json().await.context("解析二维码状态失败")?;
        let polled = polled.into_data("查询二维码状态")?;
        match polled.code {
            0 => {
                if cookies.is_empty() {
                    bail!("扫码成功，但响应中没有登录 Cookie");
                }
                store.save(&cookies)?;
                println!("登录成功，Cookie 已加密保存到 {}", store.path().display());
                return Ok(());
            }
            86101 => println!("等待扫码……"),
            86090 => println!("已扫码，等待手机确认……"),
            86038 => bail!("二维码已过期，请重新执行 `bildowner auth qr`"),
            code => bail!("二维码登录失败（{code}）：{}", polled.message),
        }
    }
}

fn extract_set_cookies(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .filter(|pair| pair.contains('='))
        .collect::<Vec<_>>()
        .join("; ")
}

fn normalize_cookie(cookie: &str) -> Result<String> {
    let pairs = cookie
        .trim()
        .split(';')
        .filter_map(|part| {
            let part = part.trim();
            (!part.is_empty()).then_some(part)
        })
        .collect::<Vec<_>>();
    if pairs.is_empty() || pairs.iter().any(|pair| !pair.contains('=')) {
        bail!("Cookie 格式不正确，应为 `name=value; name2=value2`");
    }
    Ok(pairs.join("; "))
}

fn config_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .context("找不到 APPDATA 环境变量")
    } else if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        Ok(PathBuf::from(path))
    } else {
        env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".config"))
            .context("找不到 HOME 环境变量")
    }
}

#[derive(Debug, Deserialize)]
struct QrGenerate {
    url: String,
    qrcode_key: String,
}

#[derive(Debug, Deserialize)]
struct QrPoll {
    code: i64,
    message: String,
}

#[cfg(windows)]
fn protect(clear: &[u8]) -> Result<Vec<u8>> {
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: clear.len().try_into().context("Cookie 太大")?,
        pbData: clear.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error()).context("Windows DPAPI 加密 Cookie 失败");
    }
    let bytes = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut _) };
    Ok(bytes)
}

#[cfg(windows)]
fn unprotect(encrypted: &[u8]) -> Result<Vec<u8>> {
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len().try_into().context("Cookie 文件太大")?,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error())
            .context("Windows DPAPI 解密 Cookie 失败；该文件只能由保存它的 Windows 用户读取");
    }
    let bytes = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut _) };
    Ok(bytes)
}

#[cfg(not(windows))]
fn protect(clear: &[u8]) -> Result<Vec<u8>> {
    eprintln!("警告：当前平台没有启用系统加密，Cookie 将以明文写入配置文件。");
    Ok(clear.to_vec())
}

#[cfg(not(windows))]
fn unprotect(encrypted: &[u8]) -> Result<Vec<u8>> {
    Ok(encrypted.to_vec())
}

#[cfg(test)]
mod tests {
    use super::normalize_cookie;

    #[test]
    fn normalizes_cookie_whitespace() {
        assert_eq!(
            normalize_cookie("  SESSDATA=abc ; bili_jct=def;\n").unwrap(),
            "SESSDATA=abc; bili_jct=def"
        );
    }
}
