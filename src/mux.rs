use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
use mp4::{
    AacConfig, AvcConfig, MediaConfig, MediaType, Mp4Config, Mp4Reader, Mp4Sample, Mp4Writer,
    TrackConfig, TrackType,
};

pub fn mux_avc_aac(video_path: &Path, audio_path: &Path, output_path: &Path) -> Result<()> {
    let video = open_mp4(video_path, "视频")?;
    let audio = open_mp4(audio_path, "音频")?;
    let (video_track_id, video_config) = video_track_config(&video)?;
    let (audio_track_id, audio_config) = audio_track_config(&audio)?;

    let output = File::create(output_path)
        .with_context(|| format!("无法创建临时合并文件 {}", output_path.display()))?;
    let config = Mp4Config {
        major_brand: "isom".parse().expect("static MP4 brand"),
        minor_version: 512,
        compatible_brands: ["isom", "iso2", "avc1", "mp41"]
            .into_iter()
            .map(|brand| brand.parse().expect("static MP4 brand"))
            .collect(),
        timescale: 1000,
    };
    let mut writer =
        Mp4Writer::write_start(BufWriter::new(output), &config).context("初始化 MP4 封装器失败")?;
    writer
        .add_track(&video_config)
        .context("创建视频轨道失败")?;
    writer
        .add_track(&audio_config)
        .context("创建音频轨道失败")?;

    copy_fragmented_samples(&video, video_path, video_track_id, &mut writer, 1, "视频")?;
    copy_fragmented_samples(&audio, audio_path, audio_track_id, &mut writer, 2, "音频")?;
    writer.write_end().context("写入 MP4 索引失败")?;
    writer
        .into_writer()
        .flush()
        .context("刷新 MP4 合并文件失败")?;
    Ok(())
}

fn open_mp4(path: &Path, label: &str) -> Result<Mp4Reader<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("无法打开{label}流 {}", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("无法读取{label}流大小"))?
        .len();
    Mp4Reader::read_header(BufReader::new(file), size)
        .with_context(|| format!("解析{label} M4S 失败"))
}

fn video_track_config<R: std::io::Read + std::io::Seek>(
    reader: &Mp4Reader<R>,
) -> Result<(u32, TrackConfig)> {
    let (track_id, track) = reader
        .tracks()
        .iter()
        .find(|(_, track)| track.track_type().ok() == Some(TrackType::Video))
        .context("视频 M4S 中没有视频轨道")?;
    if track.media_type().context("识别视频编码失败")? != MediaType::H264 {
        bail!("内置 MP4 合并目前仅支持 AVC/H.264 视频；请使用 --codec avc");
    }
    let config = AvcConfig {
        width: track.width(),
        height: track.height(),
        seq_param_set: track
            .sequence_parameter_set()
            .context("读取 H.264 SPS 失败")?
            .to_vec(),
        pic_param_set: track
            .picture_parameter_set()
            .context("读取 H.264 PPS 失败")?
            .to_vec(),
    };
    Ok((
        *track_id,
        TrackConfig {
            track_type: TrackType::Video,
            timescale: track.timescale(),
            language: track.language().to_owned(),
            media_conf: MediaConfig::AvcConfig(config),
        },
    ))
}

fn audio_track_config<R: std::io::Read + std::io::Seek>(
    reader: &Mp4Reader<R>,
) -> Result<(u32, TrackConfig)> {
    let (track_id, track) = reader
        .tracks()
        .iter()
        .find(|(_, track)| track.track_type().ok() == Some(TrackType::Audio))
        .context("音频 M4S 中没有音频轨道")?;
    if track.media_type().context("识别音频编码失败")? != MediaType::AAC {
        bail!("内置 MP4 合并目前仅支持 AAC 音频");
    }
    let config = AacConfig {
        bitrate: track.bitrate().max(128_000),
        profile: track.audio_profile().context("读取 AAC Profile 失败")?,
        freq_index: track.sample_freq_index().context("读取 AAC 采样率失败")?,
        chan_conf: track.channel_config().context("读取 AAC 声道配置失败")?,
    };
    Ok((
        *track_id,
        TrackConfig {
            track_type: TrackType::Audio,
            timescale: track.timescale(),
            language: track.language().to_owned(),
            media_conf: MediaConfig::AacConfig(config),
        },
    ))
}

fn copy_fragmented_samples<R: Read + Seek, W: Write + Seek>(
    reader: &Mp4Reader<R>,
    source_path: &Path,
    source_track_id: u32,
    writer: &mut Mp4Writer<W>,
    destination_track_id: u32,
    label: &str,
) -> Result<()> {
    if !reader.is_fragmented() {
        bail!("{label}输入不是 fragmented MP4");
    }
    let moof_offsets = top_level_box_offsets(source_path, *b"moof")?;
    if moof_offsets.len() != reader.moofs.len() {
        bail!("{label} M4S 的 moof 数量与解析结果不一致");
    }
    let trex = reader
        .moov
        .mvex
        .as_ref()
        .map(|mvex| &mvex.trex)
        .filter(|trex| trex.track_id == source_track_id);
    let mut source = BufReader::new(
        File::open(source_path)
            .with_context(|| format!("无法重新打开{label}流 {}", source_path.display()))?,
    );
    let mut total_samples = 0u64;

    for (moof_index, moof) in reader.moofs.iter().enumerate() {
        let moof_start = moof_offsets[moof_index];
        for traf in moof
            .trafs
            .iter()
            .filter(|traf| traf.tfhd.track_id == source_track_id)
        {
            let trun = traf
                .trun
                .as_ref()
                .with_context(|| format!("{label} traf 中缺少 trun"))?;
            let base_offset = traf.tfhd.base_data_offset.unwrap_or(moof_start);
            let mut sample_offset = add_signed_offset(base_offset, trun.data_offset.unwrap_or(0))
                .with_context(|| format!("{label} trun 数据偏移无效"))?;
            let mut decode_time = traf
                .tfdt
                .as_ref()
                .map(|tfdt| tfdt.base_media_decode_time)
                .unwrap_or(0);

            for sample_index in 0..trun.sample_count as usize {
                let sample_size = trun
                    .sample_sizes
                    .get(sample_index)
                    .copied()
                    .or(traf.tfhd.default_sample_size)
                    .or_else(|| trex.map(|trex| trex.default_sample_size))
                    .filter(|size| *size > 0)
                    .with_context(|| format!("{label}样本缺少大小"))?;
                let duration = trun
                    .sample_durations
                    .get(sample_index)
                    .copied()
                    .or(traf.tfhd.default_sample_duration)
                    .or_else(|| trex.map(|trex| trex.default_sample_duration))
                    .filter(|duration| *duration > 0)
                    .with_context(|| format!("{label}样本缺少时长"))?;
                let sample_flags = trun
                    .sample_flags
                    .get(sample_index)
                    .copied()
                    .or_else(|| {
                        (sample_index == 0)
                            .then_some(trun.first_sample_flags)
                            .flatten()
                    })
                    .or(traf.tfhd.default_sample_flags)
                    .or_else(|| trex.map(|trex| trex.default_sample_flags))
                    .unwrap_or(0);
                let rendering_offset = trun
                    .sample_cts
                    .get(sample_index)
                    .copied()
                    .map(|offset| {
                        if trun.version == 1 {
                            offset as i32
                        } else {
                            i32::try_from(offset).unwrap_or(i32::MAX)
                        }
                    })
                    .unwrap_or(0);

                let mut bytes = vec![0u8; sample_size as usize];
                source
                    .seek(SeekFrom::Start(sample_offset))
                    .and_then(|_| source.read_exact(&mut bytes))
                    .with_context(|| {
                        format!(
                            "读取{label}样本 {}（偏移 {sample_offset}，大小 {sample_size}）失败",
                            total_samples + 1
                        )
                    })?;
                let sample = Mp4Sample {
                    start_time: decode_time,
                    duration,
                    rendering_offset,
                    is_sync: sample_flags & 0x0001_0000 == 0,
                    bytes: bytes.into(),
                };
                writer
                    .write_sample(destination_track_id, &sample)
                    .with_context(|| format!("写入{label}样本 {} 失败", total_samples + 1))?;
                sample_offset += u64::from(sample_size);
                decode_time += u64::from(duration);
                total_samples += 1;
            }
        }
    }
    if total_samples == 0 {
        bail!("{label}轨道没有媒体样本");
    }
    Ok(())
}

fn add_signed_offset(base: u64, offset: i32) -> Option<u64> {
    if offset >= 0 {
        base.checked_add(offset as u64)
    } else {
        base.checked_sub(offset.unsigned_abs() as u64)
    }
}

fn top_level_box_offsets(path: &Path, target: [u8; 4]) -> Result<Vec<u64>> {
    let mut file = BufReader::new(
        File::open(path).with_context(|| format!("无法扫描 MP4 盒结构 {}", path.display()))?,
    );
    let file_size = file.get_ref().metadata()?.len();
    let mut offsets = Vec::new();
    let mut position = 0u64;
    while position + 8 <= file_size {
        file.seek(SeekFrom::Start(position))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let short_size = u32::from_be_bytes(header[0..4].try_into().expect("four bytes"));
        let box_type: [u8; 4] = header[4..8].try_into().expect("four bytes");
        let box_size = match short_size {
            0 => file_size - position,
            1 => {
                let mut extended = [0u8; 8];
                file.read_exact(&mut extended)?;
                u64::from_be_bytes(extended)
            }
            size => u64::from(size),
        };
        let header_size = if short_size == 1 { 16 } else { 8 };
        if box_size < header_size || position + box_size > file_size {
            bail!("{} 包含无效的顶层 MP4 盒大小", path.display());
        }
        if box_type == target {
            offsets.push(position);
        }
        position += box_size;
    }
    Ok(offsets)
}

#[cfg(test)]
mod tests {
    use super::add_signed_offset;

    #[test]
    fn applies_signed_fragment_offsets() {
        assert_eq!(add_signed_offset(100, 24), Some(124));
        assert_eq!(add_signed_offset(100, -24), Some(76));
        assert_eq!(add_signed_offset(10, -24), None);
    }
}
