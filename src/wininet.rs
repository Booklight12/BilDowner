use std::{
    ffi::c_void,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    ptr,
};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use windows_sys::Win32::Networking::WinInet::{
    HTTP_QUERY_CONTENT_LENGTH, HTTP_QUERY_FLAG_NUMBER, HTTP_QUERY_FLAG_NUMBER64,
    HTTP_QUERY_STATUS_CODE, HttpQueryInfoW, INTERNET_FLAG_NO_CACHE_WRITE, INTERNET_FLAG_NO_UI,
    INTERNET_FLAG_RELOAD, INTERNET_OPEN_TYPE_PRECONFIG, InternetCloseHandle, InternetOpenUrlW,
    InternetOpenW, InternetReadFile,
};

pub async fn get_text(url: String, headers: String, user_agent: &'static str) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let bytes = get_bytes(&url, &headers, user_agent)?;
        String::from_utf8(bytes).context("Windows 网络响应不是有效 UTF-8")
    })
    .await
    .context("Windows 网络读取任务异常结束")?
}

pub async fn download(
    url: String,
    destination: PathBuf,
    label: &'static str,
    force: bool,
    user_agent: &'static str,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        download_blocking(&url, &destination, label, force, user_agent)
    })
    .await
    .context("Windows 下载任务异常结束")?
}

fn get_bytes(url: &str, headers: &str, user_agent: &str) -> Result<Vec<u8>> {
    let session = open_session(user_agent)?;
    let request = open_url(&session, url, headers)?;
    ensure_success(&request)?;
    read_all(&request)
}

fn download_blocking(
    url: &str,
    destination: &Path,
    label: &str,
    force: bool,
    user_agent: &str,
) -> Result<()> {
    if destination.exists() && !force {
        println!("{label}已存在，跳过：{}", destination.display());
        return Ok(());
    }
    if force && destination.exists() {
        fs::remove_file(destination)
            .with_context(|| format!("无法覆盖已有文件 {}", destination.display()))?;
    }

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("下载文件名不是有效 UTF-8")?;
    let partial = destination.with_file_name(format!("{file_name}.part"));
    if force && partial.exists() {
        fs::remove_file(&partial)
            .with_context(|| format!("无法清理临时文件 {}", partial.display()))?;
    }
    let existing = fs::metadata(&partial).map(|meta| meta.len()).unwrap_or(0);
    let mut headers = String::from(
        "Accept: video/mp4,video/*;q=0.9,*/*;q=0.8\r\n\
         Origin: https://x.com\r\n\
         Referer: https://x.com/\r\n",
    );
    if existing > 0 {
        headers.push_str(&format!("Range: bytes={existing}-\r\n"));
    }

    let session = open_session(user_agent)?;
    let request = open_url(&session, url, &headers)?;
    let status = status_code(&request)?;
    if !(200..300).contains(&status) {
        bail!("X 视频下载地址返回 HTTP {status}");
    }
    let resumed = existing > 0 && status == 206;
    let initial = if resumed { existing } else { 0 };
    let total = content_length(&request).map(|length| length + initial);
    let bar = make_progress_bar(label, total);
    bar.set_position(initial);
    let mut downloaded = initial;

    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if resumed {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(&partial)
        .with_context(|| format!("无法写入 {}", partial.display()))?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = read_chunk(&request, &mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .context("写入下载文件失败")?;
        bar.inc(read as u64);
        downloaded += read as u64;
    }
    file.flush().context("刷新下载文件失败")?;
    drop(file);
    if let Some(expected) = total
        && downloaded != expected
    {
        bail!("X 视频下载不完整：预期 {expected} 字节，实际 {downloaded} 字节");
    }
    bar.finish_with_message(format!("{label}完成"));

    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(&partial, destination)
        .with_context(|| format!("无法完成文件 {}", destination.display()))?;
    Ok(())
}

struct InternetHandle(*mut c_void);

impl InternetHandle {
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        // SAFETY: WinINet returned this owned handle and it is closed exactly once here.
        unsafe {
            InternetCloseHandle(self.0);
        }
    }
}

fn open_session(user_agent: &str) -> Result<InternetHandle> {
    let user_agent = wide(user_agent);
    // SAFETY: All strings are valid, null-terminated UTF-16 and optional pointers are null.
    let handle = unsafe {
        InternetOpenW(
            user_agent.as_ptr(),
            INTERNET_OPEN_TYPE_PRECONFIG,
            ptr::null(),
            ptr::null(),
            0,
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("无法创建 Windows 网络会话");
    }
    Ok(InternetHandle(handle))
}

fn open_url(session: &InternetHandle, url: &str, headers: &str) -> Result<InternetHandle> {
    let url_wide = wide(url);
    let headers_wide = wide(headers);
    let flags = INTERNET_FLAG_RELOAD | INTERNET_FLAG_NO_CACHE_WRITE | INTERNET_FLAG_NO_UI;
    // SAFETY: The session is live, strings are valid UTF-16, and WinINet copies request data.
    let handle = unsafe {
        InternetOpenUrlW(
            session.as_ptr(),
            url_wide.as_ptr(),
            headers_wide.as_ptr(),
            (headers_wide.len() - 1) as u32,
            flags,
            0,
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Windows 无法连接 {url}"));
    }
    Ok(InternetHandle(handle))
}

fn ensure_success(request: &InternetHandle) -> Result<()> {
    let status = status_code(request)?;
    if !(200..300).contains(&status) {
        bail!("Windows 网络地址返回 HTTP {status}");
    }
    Ok(())
}

fn status_code(request: &InternetHandle) -> Result<u32> {
    query_u32(request, HTTP_QUERY_STATUS_CODE | HTTP_QUERY_FLAG_NUMBER)
        .context("无法读取 HTTP 状态码")
}

fn content_length(request: &InternetHandle) -> Option<u64> {
    query_u64(
        request,
        HTTP_QUERY_CONTENT_LENGTH | HTTP_QUERY_FLAG_NUMBER64,
    )
}

fn query_u32(request: &InternetHandle, query: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut length = size_of::<u32>() as u32;
    let mut index = 0u32;
    // SAFETY: The output buffer and its byte length match the requested numeric query.
    let ok = unsafe {
        HttpQueryInfoW(
            request.as_ptr(),
            query,
            (&mut value as *mut u32).cast(),
            &mut length,
            &mut index,
        )
    };
    (ok != 0).then_some(value)
}

fn query_u64(request: &InternetHandle, query: u32) -> Option<u64> {
    let mut value = 0u64;
    let mut length = size_of::<u64>() as u32;
    let mut index = 0u32;
    // SAFETY: The output buffer and its byte length match the requested numeric query.
    let ok = unsafe {
        HttpQueryInfoW(
            request.as_ptr(),
            query,
            (&mut value as *mut u64).cast(),
            &mut length,
            &mut index,
        )
    };
    (ok != 0).then_some(value)
}

fn read_all(request: &InternetHandle) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = read_chunk(request, &mut buffer)?;
        if read == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn read_chunk(request: &InternetHandle, buffer: &mut [u8]) -> Result<usize> {
    let mut read = 0u32;
    // SAFETY: The buffer is writable for the supplied length and the request handle is live.
    let ok = unsafe {
        InternetReadFile(
            request.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut read,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("Windows 网络流读取失败");
    }
    Ok(read as usize)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
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
