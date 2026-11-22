//! Disc images for the PlayStation and the PlayStation 2: track layouts
//! (CUE sheets, plain ISO files, CD or DVD), raw 2352-byte sector access,
//! subchannel data for protected discs (SBI files), and just enough ISO 9660
//! to find a disc's boot executable, its serial number and its console.
//!
//! The crate has no dependencies and no I/O of its own: files are read
//! through the [`FileReader`] trait, so the same code runs on the server
//! (local files) and in the browser (HTTP range requests).

mod cue;
mod iso9660;
mod msf;
mod sbi;

pub use cue::{CueError, parse_cue};
pub use iso9660::{IsoError, SystemCnf, find_file, read_system_cnf, serial_from_boot_path};
pub use msf::Msf;
pub use sbi::{SbiError, SubQ, Subchannel};

/// Size of a raw CD sector, as stored in BIN files.
pub const RAW_SECTOR_SIZE: usize = 2352;
/// Size of the user data of a Mode 1 or Mode 2 Form 1 sector.
pub const DATA_SECTOR_SIZE: usize = 2048;
/// The two-second lead-in before LBA 0 (MSF 00:02:00).
pub const LEAD_IN_SECTORS: u32 = 150;

/// How the sectors of a track are stored in its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackMode {
    /// CD-DA, 2352 bytes of 16-bit stereo PCM per sector.
    Audio,
    /// `MODE1/2352`: raw sectors.
    Mode1Raw,
    /// `MODE2/2352`: raw sectors, the usual PlayStation dump format.
    Mode2Raw,
    /// `MODE1/2048`, or a plain `.iso`: user data only.
    Mode1Cooked,
    /// `MODE2/2336`: everything but the sync pattern and the header.
    Mode2Cooked,
}

impl TrackMode {
    /// Bytes per sector in the image file.
    pub fn stored_size(self) -> u64 {
        match self {
            Self::Audio | Self::Mode1Raw | Self::Mode2Raw => 2352,
            Self::Mode1Cooked => 2048,
            Self::Mode2Cooked => 2336,
        }
    }

    pub fn is_audio(self) -> bool {
        self == Self::Audio
    }
}

/// One track of a disc. Positions are LBAs (LBA 0 is MSF 00:02:00).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// Track number, from 1.
    pub number: u8,
    pub mode: TrackMode,
    /// Index of the image file in [`Layout::files`].
    pub file: usize,
    /// First sector of the pregap (INDEX 00 or PREGAP). Equal to `start`
    /// when the track has no pregap.
    pub pregap_start: u32,
    /// Whether the pregap sectors are stored in the file (INDEX 00) or must
    /// be synthesised (PREGAP).
    pub pregap_in_file: bool,
    /// INDEX 01: the first sector of the track proper.
    pub start: u32,
    /// One past the last sector of the track.
    pub end: u32,
    /// Byte offset in the file of the sector at `start`.
    pub file_offset: u64,
}

/// The track layout of a disc, independent of how its files are stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Image file names, as referenced by the CUE sheet.
    pub files: Vec<String>,
    pub tracks: Vec<Track>,
}

impl Layout {
    /// A single-track data disc made from a plain ISO (2048-byte sectors) or
    /// a raw image without a CUE sheet (2352-byte sectors), detected by the
    /// file size.
    pub fn single_file(name: &str, size: u64) -> Option<Self> {
        // A size that is a multiple of both (301056 bytes) is taken as an ISO.
        let mode = if size.is_multiple_of(2048) {
            TrackMode::Mode1Cooked
        } else if size.is_multiple_of(2352) {
            TrackMode::Mode2Raw
        } else {
            return None;
        };
        let sectors = u32::try_from(size / mode.stored_size()).ok()?;
        Some(Self {
            files: vec![name.to_owned()],
            tracks: vec![Track {
                number: 1,
                mode,
                file: 0,
                pregap_start: 0,
                pregap_in_file: false,
                start: 0,
                end: sectors,
                file_offset: 0,
            }],
        })
    }

    /// One past the last sector of the disc, where the lead-out starts.
    pub fn lead_out(&self) -> u32 {
        self.tracks.last().map_or(0, |t| t.end)
    }

    /// The track that contains `lba`, pregap included.
    pub fn track_at(&self, lba: u32) -> Option<&Track> {
        self.tracks.iter().find(|t| lba >= t.pregap_start && lba < t.end)
    }

    pub fn track(&self, number: u8) -> Option<&Track> {
        self.tracks.iter().find(|t| t.number == number)
    }
}

/// Reads bytes from the image files of a disc.
pub trait FileReader {
    /// Fills `buf` from `offset` in file `file`. Short files are an error.
    fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> Result<(), ReadError>;
}

impl<T: FileReader + ?Sized> FileReader for Box<T> {
    fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        (**self).read_at(file, offset, buf)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The sector is outside of every track (lead-out, past the end).
    OutOfDisc(u32),
    /// The underlying file could not be read.
    Io(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfDisc(lba) => write!(f, "sector {lba} is outside of the disc"),
            Self::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// What was read by [`Disc::read_sector`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectorInfo {
    pub track: u8,
    pub audio: bool,
    /// Whether the sector is in the pregap (index 0) of its track.
    pub pregap: bool,
}

/// A disc: a layout, a way to read its files, and the subchannel data of
/// its protection, if any.
pub struct Disc<R> {
    pub layout: Layout,
    reader: R,
    pub subchannel: Subchannel,
}

impl<R: FileReader> Disc<R> {
    pub fn new(layout: Layout, reader: R) -> Self {
        Self {
            layout,
            reader,
            subchannel: Subchannel::default(),
        }
    }

    /// The disc with the subchannel data of an SBI file.
    #[must_use]
    pub fn with_subchannel(mut self, subchannel: Subchannel) -> Self {
        self.subchannel = subchannel;
        self
    }

    /// The subchannel Q data of a sector: from its position, but for the
    /// sectors an SBI file lists.
    pub fn sub_q(&self, lba: u32) -> SubQ {
        let (track, start, audio) = self
            .layout
            .track_at(lba)
            .map_or((1, 0, false), |t| (t.number, t.start, t.mode == TrackMode::Audio));
        let rel = Msf::from_sectors(lba.abs_diff(start));
        let abs = Msf::from_lba(lba);
        let normal = [
            if audio { 0x01 } else { 0x41 },
            msf::to_bcd(track),
            u8::from(lba >= start),
            rel.m_bcd(),
            rel.s_bcd(),
            rel.f_bcd(),
            0,
            abs.m_bcd(),
            abs.s_bcd(),
            abs.f_bcd(),
        ];
        self.subchannel.q(lba, normal)
    }

    /// Reads the raw 2352-byte sector at `lba`. Sectors that are not stored
    /// raw get a synthesised sync pattern, header and subheader, so that
    /// callers always see the layout of a Mode 2 disc. EDC/ECC are left as
    /// zeroes: nothing on the PlayStation checks them.
    pub fn read_sector(&mut self, lba: u32, out: &mut [u8; RAW_SECTOR_SIZE]) -> Result<SectorInfo, ReadError> {
        let track = self.layout.track_at(lba).ok_or(ReadError::OutOfDisc(lba))?.clone();
        let info = SectorInfo {
            track: track.number,
            audio: track.mode.is_audio(),
            pregap: lba < track.start,
        };
        if info.pregap && !track.pregap_in_file {
            out.fill(0);
            if !info.audio {
                write_header(out, lba, 2);
            }
            return Ok(info);
        }
        let size = track.mode.stored_size();
        // INDEX 00 sectors precede `file_offset` in the file.
        let offset = (track.file_offset as i64 + (i64::from(lba) - i64::from(track.start)) * size as i64) as u64;
        match track.mode {
            TrackMode::Audio | TrackMode::Mode1Raw | TrackMode::Mode2Raw => {
                self.reader.read_at(track.file, offset, out)?;
            }
            TrackMode::Mode1Cooked => {
                // Presented as Mode 2 Form 1, which is what PlayStation
                // software expects: the data then sits at offset 24.
                out.fill(0);
                write_header(out, lba, 2);
                out[16..24].copy_from_slice(&[0, 0, 0x08, 0, 0, 0, 0x08, 0]);
                self.reader
                    .read_at(track.file, offset, &mut out[24..24 + DATA_SECTOR_SIZE])?;
            }
            TrackMode::Mode2Cooked => {
                write_header(out, lba, 2);
                self.reader.read_at(track.file, offset, &mut out[16..])?;
            }
        }
        Ok(info)
    }

    /// Reads the 2048 bytes of user data of a Mode 1 or Mode 2 Form 1 sector.
    pub fn read_data(&mut self, lba: u32, out: &mut [u8; DATA_SECTOR_SIZE]) -> Result<(), ReadError> {
        let mut raw = [0u8; RAW_SECTOR_SIZE];
        self.read_sector(lba, &mut raw)?;
        let start = if raw[15] == 1 { 16 } else { 24 };
        out.copy_from_slice(&raw[start..start + DATA_SECTOR_SIZE]);
        Ok(())
    }

    pub fn reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }
}

/// Writes the sync pattern and the header (BCD MSF and mode) of a sector.
fn write_header(out: &mut [u8; RAW_SECTOR_SIZE], lba: u32, mode: u8) {
    out[0] = 0;
    out[1..11].fill(0xff);
    out[11] = 0;
    let msf = Msf::from_lba(lba);
    out[12] = msf.m_bcd();
    out[13] = msf.s_bcd();
    out[14] = msf.f_bcd();
    out[15] = mode;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files as in-memory byte vectors.
    pub struct MemReader(pub Vec<Vec<u8>>);

    impl FileReader for MemReader {
        fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
            let data = self.0.get(file).ok_or_else(|| ReadError::Io("no such file".into()))?;
            let start = usize::try_from(offset).map_err(|e| ReadError::Io(e.to_string()))?;
            let src = data
                .get(start..start + buf.len())
                .ok_or_else(|| ReadError::Io("short read".into()))?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn iso_is_presented_as_mode2_form1() {
        let mut iso = vec![0u8; 2048 * 20];
        iso[2048 * 16..2048 * 16 + 5].copy_from_slice(b"\x01CD00");
        let layout = Layout::single_file("game.iso", iso.len() as u64).unwrap();
        assert_eq!(layout.tracks[0].mode, TrackMode::Mode1Cooked);
        let mut disc = Disc::new(layout, MemReader(vec![iso]));
        let mut raw = [0u8; RAW_SECTOR_SIZE];
        disc.read_sector(16, &mut raw).unwrap();
        assert_eq!(&raw[12..16], &[0x00, 0x02, 0x16, 2]);
        assert_eq!(raw[18], 0x08);
        assert_eq!(&raw[24..29], b"\x01CD00");
        let mut data = [0u8; DATA_SECTOR_SIZE];
        disc.read_data(16, &mut data).unwrap();
        assert_eq!(&data[..5], b"\x01CD00");
    }

    #[test]
    fn raw_single_file_is_detected() {
        let layout = Layout::single_file("game.bin", 2352 * 1000).unwrap();
        assert_eq!(layout.tracks[0].mode, TrackMode::Mode2Raw);
        assert_eq!(layout.lead_out(), 1000);
        assert!(Layout::single_file("odd.bin", 1234).is_none());
    }
}
