//! CUE sheet parsing.

use crate::{Layout, Track, TrackMode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CueError {
    Syntax { line: usize, message: String },
    UnsupportedFileType(String),
    UnsupportedTrackMode(String),
    MissingFile(String),
    NoTracks,
}

impl std::fmt::Display for CueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syntax { line, message } => write!(f, "line {line}: {message}"),
            Self::UnsupportedFileType(t) => write!(f, "unsupported FILE type {t} (only BINARY is)"),
            Self::UnsupportedTrackMode(m) => write!(f, "unsupported TRACK mode {m}"),
            Self::MissingFile(name) => write!(f, "file {name} is missing"),
            Self::NoTracks => f.write_str("the CUE sheet has no tracks"),
        }
    }
}

impl std::error::Error for CueError {}

struct CueTrack {
    number: u8,
    mode: TrackMode,
    index0: Option<u32>,
    index1: Option<u32>,
    pregap: u32,
    postgap: u32,
}

struct CueFile {
    name: String,
    tracks: Vec<CueTrack>,
}

/// Parses a CUE sheet. `file_size` returns the size in bytes of a file the
/// sheet references, or `None` when it does not exist.
pub fn parse_cue(text: &str, mut file_size: impl FnMut(&str) -> Option<u64>) -> Result<Layout, CueError> {
    let files = parse_lines(text)?;
    let mut layout = Layout {
        files: Vec::new(),
        tracks: Vec::new(),
    };
    // The LBA where the next file starts.
    let mut next_file_lba = 0u32;
    for file in files {
        let size = file_size(&file.name).ok_or_else(|| CueError::MissingFile(file.name.clone()))?;
        let file_index = layout.files.len();
        layout.files.push(file.name);
        // The LBA of the first sector stored in the file; synthetic pregaps
        // shift it (and every later sector of the file) forward.
        let mut file_lba = next_file_lba;
        // Byte offset and INDEX 01 (in sectors from the file start) of the
        // previous track of this file.
        let mut prev: Option<(u64, u32, u64)> = None;
        let first_track = layout.tracks.len();
        for t in file.tracks {
            let index1 = t.index1.ok_or_else(|| CueError::Syntax {
                line: 0,
                message: format!("track {} has no INDEX 01", t.number),
            })?;
            file_lba += t.pregap;
            let start = file_lba + index1;
            let (pregap_start, pregap_in_file) = match t.index0 {
                Some(i0) => (file_lba + i0, true),
                None => (start - t.pregap, false),
            };
            let size_of = t.mode.stored_size();
            let file_offset = match prev {
                None => u64::from(index1) * size_of,
                Some((offset, prev_index1, prev_size)) => {
                    offset + u64::from(index1.saturating_sub(prev_index1)) * prev_size
                }
            };
            if layout.tracks.len() > first_track {
                let last = layout.tracks.last_mut().expect("a previous track");
                last.end = pregap_start;
            }
            prev = Some((file_offset, index1, size_of));
            layout.tracks.push(Track {
                number: t.number,
                mode: t.mode,
                file: file_index,
                pregap_start,
                pregap_in_file,
                start,
                end: start,
                file_offset,
            });
            next_file_lba = t.postgap;
        }
        if layout.tracks.len() == first_track {
            continue;
        }
        let last = layout.tracks.last_mut().expect("a track of this file");
        let sectors = size.saturating_sub(last.file_offset) / last.mode.stored_size();
        last.end = last.start + u32::try_from(sectors).unwrap_or(u32::MAX - last.start);
        next_file_lba += last.end;
    }
    if layout.tracks.is_empty() {
        return Err(CueError::NoTracks);
    }
    Ok(layout)
}

fn parse_lines(text: &str) -> Result<Vec<CueFile>, CueError> {
    let mut files: Vec<CueFile> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        let err = |message: String| CueError::Syntax { line, message };
        let raw = raw.trim().trim_start_matches('\u{feff}');
        let Some((command, rest)) = split_word(raw) else {
            continue;
        };
        match command.to_ascii_uppercase().as_str() {
            "FILE" => {
                let (name, kind) = split_quoted(rest).ok_or_else(|| err("FILE without a name".into()))?;
                let kind = kind.trim().to_ascii_uppercase();
                if kind != "BINARY" {
                    return Err(CueError::UnsupportedFileType(kind));
                }
                files.push(CueFile {
                    name: name.to_owned(),
                    tracks: Vec::new(),
                });
            }
            "TRACK" => {
                let file = files.last_mut().ok_or_else(|| err("TRACK before FILE".into()))?;
                let (number, mode) = split_word(rest).ok_or_else(|| err("TRACK without a number".into()))?;
                let number = number.parse().map_err(|_| err(format!("bad track number {number}")))?;
                let mode = match mode.trim().to_ascii_uppercase().as_str() {
                    "AUDIO" => TrackMode::Audio,
                    "MODE1/2352" => TrackMode::Mode1Raw,
                    "MODE2/2352" | "CDI/2352" => TrackMode::Mode2Raw,
                    "MODE1/2048" => TrackMode::Mode1Cooked,
                    "MODE2/2336" | "CDI/2336" => TrackMode::Mode2Cooked,
                    other => return Err(CueError::UnsupportedTrackMode(other.to_owned())),
                };
                file.tracks.push(CueTrack {
                    number,
                    mode,
                    index0: None,
                    index1: None,
                    pregap: 0,
                    postgap: 0,
                });
            }
            "INDEX" | "PREGAP" | "POSTGAP" => {
                let track = files
                    .last_mut()
                    .and_then(|f| f.tracks.last_mut())
                    .ok_or_else(|| err(format!("{command} before TRACK")))?;
                if command.eq_ignore_ascii_case("INDEX") {
                    let (index, time) = split_word(rest).ok_or_else(|| err("INDEX without a number".into()))?;
                    let time = parse_msf(time).ok_or_else(|| err(format!("bad time {time}")))?;
                    match index.parse::<u8>() {
                        Ok(0) => track.index0 = Some(time),
                        Ok(1) => track.index1 = Some(time),
                        Ok(_) => {}
                        Err(_) => return Err(err(format!("bad index {index}"))),
                    }
                } else {
                    let time = parse_msf(rest).ok_or_else(|| err(format!("bad time {rest}")))?;
                    if command.eq_ignore_ascii_case("PREGAP") {
                        track.pregap = time;
                    } else {
                        track.postgap = time;
                    }
                }
            }
            // REM, CATALOG, FLAGS, ISRC, TITLE, PERFORMER, SONGWRITER, ...
            _ => {}
        }
    }
    Ok(files)
}

fn split_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    Some(s.split_once(char::is_whitespace).unwrap_or((s, "")))
}

/// Splits a possibly quoted file name from the rest of the line.
fn split_quoted(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        Some((&rest[..end], &rest[end + 1..]))
    } else {
        // Unquoted: the type is the last word.
        let (name, kind) = s.trim_end().rsplit_once(char::is_whitespace)?;
        Some((name.trim_end(), kind))
    }
}

fn parse_msf(s: &str) -> Option<u32> {
    let mut parts = s.trim().split(':').map(|p| p.parse::<u32>().ok());
    let (m, sec, f) = (parts.next()??, parts.next()??, parts.next()??);
    (parts.next().is_none() && sec < 60 && f < 75).then_some((m * 60 + sec) * 75 + f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 2352;

    #[test]
    fn single_bin_with_audio_tracks() {
        let cue = "FILE \"Game (USA).bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n    INDEX 01 00:00:00\r\n  TRACK 02 AUDIO\r\n    INDEX 00 10:00:00\r\n    INDEX 01 10:02:00\r\n";
        let layout = parse_cue(cue, |_| Some(S * 50_000)).unwrap();
        assert_eq!(layout.files, ["Game (USA).bin"]);
        let [data, audio] = &layout.tracks[..] else { panic!() };
        assert_eq!((data.start, data.end), (0, 45_000));
        assert_eq!((audio.pregap_start, audio.start, audio.end), (45_000, 45_150, 50_000));
        assert!(audio.pregap_in_file);
        assert_eq!(audio.file_offset, 45_150 * S);
    }

    #[test]
    fn one_bin_per_track() {
        let cue = "FILE \"t1.bin\" BINARY\n TRACK 01 MODE2/2352\n  INDEX 01 00:00:00\nFILE \"t2.bin\" BINARY\n TRACK 02 AUDIO\n  INDEX 00 00:00:00\n  INDEX 01 00:02:00\nFILE \"t3.bin\" BINARY\n TRACK 03 AUDIO\n  PREGAP 00:02:00\n  INDEX 01 00:00:00\n";
        let layout = parse_cue(cue, |name| match name {
            "t1.bin" => Some(S * 1000),
            "t2.bin" => Some(S * 500),
            "t3.bin" => Some(S * 300),
            _ => None,
        })
        .unwrap();
        let [t1, t2, t3] = &layout.tracks[..] else { panic!() };
        assert_eq!((t1.start, t1.end), (0, 1000));
        assert_eq!(
            (t2.pregap_start, t2.start, t2.end, t2.file_offset),
            (1000, 1150, 1500, 150 * S)
        );
        assert_eq!(
            (t3.pregap_start, t3.start, t3.end, t3.file_offset),
            (1500, 1650, 1950, 0)
        );
        assert!(!t3.pregap_in_file);
        assert_eq!(layout.lead_out(), 1950);
    }

    #[test]
    fn errors() {
        assert_eq!(
            parse_cue("FILE \"a.bin\" BINARY\n", |_| Some(0)),
            Err(CueError::NoTracks)
        );
        assert_eq!(
            parse_cue("FILE a.bin BINARY\nTRACK 01 MODE2/2352\nINDEX 01 00:00:00\n", |_| None),
            Err(CueError::MissingFile("a.bin".into()))
        );
        assert!(matches!(
            parse_cue("FILE a.wav WAVE\n", |_| Some(0)),
            Err(CueError::UnsupportedFileType(_))
        ));
    }
}
