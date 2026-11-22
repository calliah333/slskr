//! Read only audio properties needed by Soulseek, without loading tags or pictures.

use std::{
    cell::OnceCell,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use lofty::{
    config::ParseOptions,
    file::{AudioFile, FileType},
    probe::Probe,
};

use super::{Attribute, AttributeType, NamedAttribute};

pub(super) fn read(
    cache: &OnceCell<Option<Dir>>,
    root: &Path,
    path: &Path,
    extension: &str,
) -> Vec<Attribute> {
    let kind = match extension {
        "aac" => FileType::Aac,
        "aiff" => FileType::Aiff,
        "ape" => FileType::Ape,
        "flac" => FileType::Flac,
        "aax" | "m4a" | "m4b" | "m4p" | "m4v" | "mp4" => FileType::Mp4,
        "mp3" => FileType::Mpeg,
        "mpc" | "mpp" => FileType::Mpc,
        "ogg" | "oga" => FileType::Vorbis,
        "wav" => FileType::Wav,
        "wv" => FileType::WavPack,
        _ => return Vec::new(),
    };
    let directory = cache.get_or_init(|| Dir::open_ambient_dir(root, ambient_authority()).ok());
    let attributes = directory.as_ref().and_then(|directory| {
        let relative = path.strip_prefix(root).ok()?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = directory.open_with(relative, &options).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        read_properties(BufReader::new(file), kind)
    });
    attributes.unwrap_or_default()
}

fn read_properties(mut reader: impl Read + Seek, kind: FileType) -> Option<Vec<Attribute>> {
    let options = ParseOptions::new().read_tags(false).read_cover_art(false);
    let probe = Probe::with_file_type(&mut reader, kind).options(options);
    // Ogg extensions can contain Vorbis, Opus or Speex.
    let tagged = if kind == FileType::Vorbis {
        probe.guess_file_type().ok()?
    } else {
        probe
    }
    .read()
    .ok()?;
    let properties = tagged.properties();
    let mut attributes = Vec::with_capacity(3);
    let mut push = |kind, value: Option<u32>| {
        if let Some(value) = value.and_then(|value| i32::try_from(value).ok()) {
            attributes.push(Attribute {
                kind: AttributeType::Named(kind),
                value,
            });
        }
    };
    let length = u32::try_from(properties.duration().as_secs()).ok();
    if let Some(depth) = properties.bit_depth().filter(|depth| *depth > 0) {
        push(NamedAttribute::Length, length);
        push(
            NamedAttribute::SampleRate,
            properties.sample_rate().filter(|rate| *rate > 0),
        );
        push(NamedAttribute::BitDepth, Some(u32::from(depth)));
    } else {
        push(
            NamedAttribute::BitRate,
            properties.audio_bitrate().filter(|rate| *rate > 0),
        );
        push(NamedAttribute::Length, length);
        let vbr = match kind {
            FileType::Mpeg => mpeg_vbr(&mut reader),
            FileType::Aac => adts_vbr(&mut reader),
            _ => None,
        };
        push(NamedAttribute::VariableBitRate, vbr.map(u32::from));
    }
    Some(attributes)
}

fn audio_start(reader: &mut (impl Read + Seek)) -> Option<u64> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut id3 = [0; 10];
    reader.read_exact(&mut id3).ok()?;
    if &id3[..3] != b"ID3" {
        return Some(0);
    }
    if id3[6..].iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    let body = id3[6..]
        .iter()
        .fold(0u64, |value, byte| (value << 7) | u64::from(*byte));
    let footer = if id3[3] == 4 && id3[5] & 0x10 != 0 {
        10
    } else {
        0
    };
    Some(10 + body + footer)
}

fn mpeg_vbr(reader: &mut (impl Read + Seek)) -> Option<bool> {
    let start = audio_start(reader)?;
    reader.seek(SeekFrom::Start(start)).ok()?;
    let mut header = [0; 4];
    reader.read_exact(&mut header).ok()?;
    // Only a Layer III frame has this side-information layout.
    let version = (header[1] >> 3) & 3;
    if header[0] != 0xff
        || header[1] & 0xe6 != 0xe2
        || version == 1
        || matches!(header[2] >> 4, 0 | 15)
        || header[2] & 0x0c == 0x0c
    {
        return None;
    }
    let mono = header[3] >> 6 == 3;
    let side_info = match (version == 3, mono) {
        (true, true) | (false, false) => 17,
        (true, false) => 32,
        (false, true) => 9,
    };
    let crc = if header[1] & 1 == 0 { 2 } else { 0 };
    let mut marker = [0; 16];
    reader
        .seek(SeekFrom::Start(start + 4 + crc + side_info))
        .ok()?;
    reader.read_exact(&mut marker).ok()?;
    if matches!(&marker[..4], b"Xing" | b"Info") {
        // The total byte count follows the optional frame count.
        let flags = u32::from_be_bytes(marker[4..8].try_into().ok()?);
        let size_offset = if flags & 1 == 1 { 12 } else { 8 };
        return (flags & 2 != 0 && marker[size_offset..size_offset + 4] != [0; 4])
            .then_some(&marker[..4] == b"Xing");
    }
    reader.seek(SeekFrom::Start(start + 36)).ok()?;
    reader.read_exact(&mut marker).ok()?;
    (&marker[..6] == b"VBRI\x00\x01" && marker[10..14] != [0; 4]).then_some(true)
}

fn adts_vbr(reader: &mut (impl Read + Seek)) -> Option<bool> {
    let start = audio_start(reader)?;
    reader.seek(SeekFrom::Start(start)).ok()?;
    let mut header = [0; 7];
    reader.read_exact(&mut header).ok()?;
    if header[0] != 0xff || header[1] & 0xf6 != 0xf0 {
        return None;
    }
    // ADTS uses the all-ones buffer fullness sentinel for variable bitrate.
    let fullness = (u16::from_be_bytes([header[5], header[6]]) >> 2) & 0x7ff;
    Some(fullness == 0x7ff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use soulseek::FileAttributeType::{BitDepth, Duration, SampleRate};
    use std::io::Cursor;

    #[test]
    fn lossless_audio_attributes_follow_wire_order() {
        let mut wav = Vec::from(*b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0");
        wav.extend(8_000u32.to_le_bytes());
        wav.extend(16_000u32.to_le_bytes());
        wav.extend([2, 0, 16, 0]);
        wav.extend(b"data");
        wav.extend(16_000u32.to_le_bytes());
        wav.resize(wav.len() + 16_000, 0);
        let riff_size = (wav.len() as u32 - 8).to_le_bytes();
        wav[4..8].copy_from_slice(&riff_size);
        let attributes = read_properties(Cursor::new(wav), FileType::Wav).unwrap();
        assert_eq!(
            attributes
                .iter()
                .map(|a| (a.wire().kind, a.value))
                .collect::<Vec<_>>(),
            [(Duration, 1), (SampleRate, 8_000), (BitDepth, 16)]
        );
        assert!(read_properties(Cursor::new(b"not a WAV file"), FileType::Wav).is_none());
    }

    #[test]
    fn mpeg_vbr_is_only_reported_with_a_valid_marker() {
        let mut frame = [0; 64];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        assert_eq!(mpeg_vbr(&mut Cursor::new(frame)), None);
        frame[36..52].copy_from_slice(b"Xing\0\0\0\x03\0\0\0\x02\0\0\x40\0");
        assert_eq!(mpeg_vbr(&mut Cursor::new(frame)), Some(true));
        frame[36..40].copy_from_slice(b"Info");
        assert_eq!(mpeg_vbr(&mut Cursor::new(frame)), Some(false));
    }
}
