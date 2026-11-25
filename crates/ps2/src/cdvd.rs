//! The CD/DVD drive's controller (CDVD), at 0x1F402000 on the IOP: "N"
//! commands drive the disc (seek, read: the sectors go to IOP DMA channel
//! 3), "S" commands ask the MechaCon (the drive's microcontroller) about
//! itself and the clock. N commands take the time the drive would, and
//! raise the CDVD interrupt when done.
//!
//! Register and command facts are from ps2tek (psi-rockin.github.io/ps2tek).

use std::collections::VecDeque;

use texel_disc::{DATA_SECTOR_SIZE, Disc, FileReader, RAW_SECTOR_SIZE};

/// INTR_STAT: data read (with a completed read), an N command completed.
const INTR_DATA_READY: u8 = 0x01;
const INTR_COMMAND_COMPLETE: u8 = 0x02;
/// N command status: ready for a command, busy with one.
const N_READY: u8 = 0x40;
const N_BUSY: u8 = 0x80;
/// S status: no result to read.
const S_RESULT_EMPTY: u8 = 0x40;

/// Drive status (1F40200Ah): its bits are tray open, spinning, reading,
/// paused, seeking.
mod drive {
    pub const STOPPED: u8 = 0x00;
    pub const READING: u8 = 0x06;
    pub const PAUSED: u8 = 0x0A;
    pub const SEEKING: u8 = 0x12;
}

/// The IOP's clock, and the drive's read speeds (bytes per second): 24x CD,
/// 4x DVD.
const IOP_CLOCK: u64 = 36_864_000;
const CD_SPEED: u64 = 24 * 153_600;
const DVD_SPEED: u64 = 4 * 1_382_400;

/// IOP cycles in `ms` milliseconds.
const fn ms(ms: u64) -> u64 {
    IOP_CLOCK / 1000 * ms
}

/// A disc's image, read through its files.
pub type Medium = Disc<Box<dyn FileReader>>;

/// What is in the drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscKind {
    /// A PlayStation 2 CD.
    Cd,
    /// A PlayStation 2 DVD.
    Dvd,
}

impl DiscKind {
    /// The disc type register's value.
    fn code(self) -> u8 {
        match self {
            Self::Cd => 0x12,
            Self::Dvd => 0x14,
        }
    }
}

/// The disc in the drive.
pub struct Inserted {
    pub medium: Medium,
    pub kind: DiscKind,
}

impl std::fmt::Debug for Inserted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} disc of {} sectors", self.kind, self.medium.layout.lead_out())
    }
}

/// A read under way: from `lsn`, `left` sectors, of `block` bytes each
/// (2064: DVD blocks, with their header), the next one in `wait` cycles.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct Read {
    lsn: u32,
    left: u32,
    block: usize,
    wait: u64,
}

/// The console's region, as the drive's EEPROM records it: the zone (bit
/// number: Japan 0, USA 1, Europe 2, Oceania 3, Asia 4, Russia 5, China 6,
/// Mexico 7), and the region parameters (code, language, and so on).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConsoleRegion {
    pub zone: u8,
    pub params: [u8; 8],
}

impl ConsoleRegion {
    /// The defaults of a console of the BIOS's region, from its ROMVER letter.
    pub fn for_bios(region: crate::bios::Region) -> Self {
        use crate::bios::Region;
        let (zone, params): (u8, &[u8; 7]) = match region {
            Region::Japan => (0, b"JJjpnJJ"),
            Region::NorthAmerica => (1, b"AAengAU"),
            Region::Europe => (2, b"EEengEE"),
            Region::Asia => (4, b"HHengJA"),
            Region::China => (6, b"CCschJC"),
            // Development BIOSes and the like: as an American console.
            Region::Unknown => (1, b"AAengAU"),
        };
        let mut p = [0u8; 8];
        p[..7].copy_from_slice(params);
        Self { zone, params: p }
    }
}

impl Default for ConsoleRegion {
    fn default() -> Self {
        Self::for_bios(crate::bios::Region::Unknown)
    }
}

/// The EEPROM's layout, for BIOS 1.70 and later: offsets of what it holds.
mod nvm {
    pub const SIZE: usize = 1024;
    pub const CONFIG0: usize = 0x270;
    pub const CONFIG1: usize = 0x2B0;
    pub const CONFIG2: usize = 0x200;
    pub const ILINK_ID: usize = 0x1E0;
    pub const MODEL_NUMBER: usize = 0x1B0;
    pub const REGION_PARAMS: usize = 0x180;
    pub const MAC: usize = 0x198;
}

/// The S commands' result sizes, in bytes (status included), from PCSX2's
/// command table; the commands not listed answer one status byte.
fn result_size(cmd: u8) -> usize {
    match cmd {
        0x02 => 11,
        0x08 | 0x91 => 8,
        0x0A | 0x20 => 3,
        0x12 | 0x17 | 0x37 | 0x45 => 9,
        0x1E => 5,
        0x22 => 10,
        0x27 => 13,
        0x32 => 2,
        0x36 => 15,
        0x41 => 16,
        0xEF => 3,
        0xFD => 6,
        _ => 1,
    }
}

/// Default language settings (the second config block's PS2 part), by
/// zone: English everywhere but in Japan and China.
fn language_defaults(zone: u8) -> [u8; 16] {
    let (a, b, last) = match zone {
        0 => (0x20, 0x20, 0x30),
        6 => (0x30, 0x2B, 0x4B),
        _ => (0x30, 0x21, 0x41),
    };
    let mut l = [0u8; 16];
    l[0] = a;
    l[1] = b;
    l[5] = 0x70;
    l[15] = last;
    l
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Cdvd {
    pub region: ConsoleRegion,
    /// The disc in the drive, if any.
    #[serde(skip)]
    pub disc: Option<Inserted>,
    /// Drive status, and its sticky copy (the OR of the states since S
    /// command 05h).
    drive: u8,
    sticky: u8,
    /// The read position.
    lsn: u32,
    /// An N command under way: cycles until it completes (reads complete
    /// when their last sector is read).
    busy: Option<u64>,
    read: Option<Read>,
    /// Error code of the last N command (0: none).
    error: u8,
    /// Blocks read, waiting for DMA channel 3.
    pub blocks: VecDeque<Vec<u8>>,
    /// Sectors read, for diagnostics.
    pub sectors_read: u64,
    /// The EEPROM: region, console IDs and the system configuration.
    pub nvm: Box<[u8]>,
    /// The open config block: (writing, block, blocks, index).
    config: (bool, u8, u8, u8),
    n_command: u8,
    n_params: Vec<u8>,
    intr_stat: u8,
    s_command: u8,
    s_params: Vec<u8>,
    s_result: VecDeque<u8>,
    /// The clock, as the RTC reports it: seconds since the start, from 1
    /// January 2000 (no date of the host: runs are reproducible).
    pub rtc_seconds: u64,
    /// S commands not emulated: command → count.
    #[serde(skip)]
    pub unknown_s: std::collections::BTreeMap<u16, u64>,
    /// Register accesses, for diagnostics: (write, register) → count.
    #[serde(skip)]
    pub accesses: std::collections::BTreeMap<(bool, u32), u64>,
    /// Where S commands are written from: IOP PC → count.
    #[serde(skip)]
    pub s_callers: std::collections::BTreeMap<u32, u64>,
    /// The last N and S commands issued, for diagnostics.
    #[serde(skip)]
    pub commands: VecDeque<(char, u8, Vec<u8>)>,
}

impl Default for Cdvd {
    fn default() -> Self {
        Self::new(ConsoleRegion::default())
    }
}

fn bcd(v: u64) -> u8 {
    (((v / 10) % 10) << 4 | (v % 10)) as u8
}

impl Cdvd {
    /// A drive with a fresh EEPROM, for a console of this region.
    pub fn new(region: ConsoleRegion) -> Self {
        let mut nvm = vec![0u8; nvm::SIZE].into_boxed_slice();
        nvm[nvm::REGION_PARAMS..nvm::REGION_PARAMS + 8].copy_from_slice(&region.params);
        nvm[nvm::ILINK_ID..nvm::ILINK_ID + 10]
            .copy_from_slice(&[0x00, 0xAC, 0xFF, 0xFF, 0xFF, 0xFF, 0xB9, 0x86, 0x00, 0x18]);
        nvm[nvm::CONFIG1 + 0x10..nvm::CONFIG1 + 0x20].copy_from_slice(&language_defaults(region.zone));
        Self {
            region,
            disc: None,
            drive: drive::STOPPED,
            sticky: drive::STOPPED,
            lsn: 0,
            busy: None,
            read: None,
            error: 0,
            blocks: VecDeque::new(),
            sectors_read: 0,
            nvm,
            config: (false, 0, 0, 0),
            n_command: 0,
            n_params: Vec::new(),
            intr_stat: 0,
            s_command: 0,
            s_params: Vec::new(),
            s_result: VecDeque::new(),
            rtc_seconds: 0,
            unknown_s: std::collections::BTreeMap::new(),
            accesses: std::collections::BTreeMap::new(),
            s_callers: std::collections::BTreeMap::new(),
            commands: VecDeque::new(),
        }
    }

    /// The offset of config block `block`, 16-byte line `index`, if valid.
    fn config_line(&self) -> Option<usize> {
        let (_, block, blocks, index) = self.config;
        let (base, lines) = match block {
            0 => (nvm::CONFIG0, 4),
            1 => (nvm::CONFIG1, 2),
            2 => (nvm::CONFIG2, 7),
            _ => return None,
        };
        (index < blocks && index < lines).then_some(base + 16 * usize::from(index))
    }

    /// `reg`: the offset from 0x1F402000. Returns whether the IOP should
    /// get a CDVD interrupt (now: N commands raise it when they complete,
    /// from `advance`).
    pub fn write(&mut self, reg: u32, v: u8) -> bool {
        *self.accesses.entry((true, reg)).or_insert(0) += 1;
        match reg {
            0x04 => {
                self.log('N', v);
                self.n_command = v;
                self.n_command();
                self.n_params.clear();
            }
            0x05 => self.n_params.push(v),
            // BREAK: the N command under way stops.
            0x07 => {
                if self.busy.take().is_some() {
                    self.read = None;
                    self.drive = drive::PAUSED;
                    self.intr_stat |= INTR_COMMAND_COMPLETE;
                    return true;
                }
            }
            // Interrupt reasons: written 1 to acknowledge.
            0x08 => self.intr_stat &= !v,
            0x16 => {
                self.log('S', v);
                self.s_command = v;
                self.s_command();
                self.s_params.clear();
            }
            0x17 => self.s_params.push(v),
            _ => {}
        }
        false
    }

    /// The drive's state, for diagnostics.
    pub fn state(&self) -> String {
        format!(
            "drive {:02X} sticky {:02X} lsn {} busy {:?} read {:?} error {} I_STAT {:02X} blocks waiting {}",
            self.drive,
            self.sticky,
            self.lsn,
            self.busy,
            self.read,
            self.error,
            self.intr_stat,
            self.blocks.len()
        )
    }

    fn log(&mut self, kind: char, cmd: u8) {
        if self.commands.len() == 512 {
            self.commands.pop_front();
        }
        let params = if kind == 'N' { &self.n_params } else { &self.s_params };
        self.commands.push_back((kind, cmd, params.clone()));
    }

    pub fn read(&mut self, reg: u32) -> u8 {
        *self.accesses.entry((false, reg)).or_insert(0) += 1;
        match reg {
            0x04 => self.n_command,
            0x05 => {
                if self.busy.is_some() {
                    N_BUSY
                } else {
                    N_READY
                }
            }
            0x06 => self.error,
            0x08 => self.intr_stat,
            0x0A => self.drive,
            0x0B => self.sticky,
            0x0F => self.disc.as_ref().map_or(0, |d| d.kind.code()),
            0x16 => self.s_command,
            0x17 => {
                if self.s_result.is_empty() {
                    S_RESULT_EMPTY
                } else {
                    0
                }
            }
            0x18 => self.s_result.pop_front().unwrap_or(0),
            _ => 0,
        }
    }

    /// Puts `disc` in the drive (`None`: empties it). The tray is not seen
    /// opening.
    pub fn insert(&mut self, disc: Option<Inserted>) {
        self.disc = disc;
        self.read = None;
        self.drive = drive::STOPPED;
        self.lsn = 0;
    }

    fn set_drive(&mut self, status: u8) {
        self.drive = status;
        self.sticky |= status;
    }

    /// A parameter word of the N command, little-endian, from byte `at`.
    fn n_word(&self, at: usize) -> u32 {
        u32::from_le_bytes(std::array::from_fn(|k| self.n_params.get(at + k).copied().unwrap_or(0)))
    }

    /// IOP cycles to read one block of `size` bytes.
    fn block_time(&self, size: usize) -> u64 {
        let speed = match self.disc.as_ref().map(|d| d.kind) {
            Some(DiscKind::Dvd) => DVD_SPEED,
            _ => CD_SPEED,
        };
        IOP_CLOCK * size as u64 / speed
    }

    /// IOP cycles to move to `lsn` (spinning up first if stopped).
    fn seek_time(&self, lsn: u32, block: usize) -> u64 {
        if self.drive == drive::STOPPED {
            return ms(333);
        }
        let dvd = matches!(self.disc.as_ref().map(|d| d.kind), Some(DiscKind::Dvd));
        let delta = lsn.abs_diff(self.lsn);
        let (contiguous, fast) = if dvd { (16, 14_764) } else { (8, 4_371) };
        // ps2tek gives about 30 ms for a short seek and 100 ms for a long
        // one (an emulator's estimates); seeks here take half that. With the
        // full figures, Jak and Daxter's sound driver (OVERLORD) loses a race
        // against the EE's first request: its main thread must collect a
        // message from its loader thread before that request arrives.
        if delta < contiguous {
            self.block_time(block) * u64::from(delta)
        } else if delta < fast {
            ms(15)
        } else {
            ms(50)
        }
    }

    /// Starts N command `n_command` with `n_params`.
    fn n_command(&mut self) {
        self.error = 0;
        self.read = None;
        let has_disc = self.disc.is_some();
        // Commands that need no disc take a moment.
        let mut wait = ms(1);
        match self.n_command {
            // Standby: to sector 0, spinning, paused.
            0x02 if has_disc => {
                wait = self.seek_time(0, DATA_SECTOR_SIZE).max(64);
                self.lsn = 0;
                self.set_drive(drive::SEEKING);
            }
            // Stop.
            0x03 => {
                wait = ms(166);
                self.lsn = 0;
                self.set_drive(drive::STOPPED);
            }
            // Seek.
            0x05 if has_disc => {
                let lsn = self.n_word(0);
                wait = self.seek_time(lsn, DATA_SECTOR_SIZE).max(64);
                self.lsn = lsn;
                self.set_drive(drive::SEEKING);
            }
            // ReadCd: blocks of 2048 bytes (byte 10: 1, 2328; 2, 2340);
            // ReadDvd: of 2064 bytes.
            0x06 | 0x08 if has_disc => {
                let block = if self.n_command == 0x08 {
                    2064
                } else {
                    match self.n_params.get(10) {
                        Some(1) => 2328,
                        Some(2) => 2340,
                        _ => DATA_SECTOR_SIZE,
                    }
                };
                let lsn = self.n_word(0);
                let count = self.n_word(4);
                let seek = self.seek_time(lsn, block);
                self.lsn = lsn;
                self.read = Some(Read {
                    lsn,
                    left: count,
                    block,
                    wait: seek + self.block_time(block),
                });
                self.set_drive(drive::READING);
                self.busy = Some(u64::MAX);
                return;
            }
            // GetToc: one block, by DMA.
            0x09 if has_disc => {
                let toc = self.toc();
                self.blocks.push_back(toc);
            }
            // Reads and seeks without a disc fail.
            0x02 | 0x05 | 0x06 | 0x08 | 0x09 => self.error = 0x01,
            _ => {}
        }
        self.busy = Some(wait);
    }

    /// The table of contents GetToc gives: for a DVD, its layer's start
    /// and end; for a CD, its one data track.
    fn toc(&mut self) -> Vec<u8> {
        let Some(disc) = &self.disc else {
            return Vec::new();
        };
        let end = disc.medium.layout.lead_out();
        match disc.kind {
            DiscKind::Dvd => {
                let mut toc = vec![0u8; 2064];
                // Single layer: book type DVD-ROM, one layer, its data area
                // from 0x30000 to its end (sector numbers + 0x30000).
                toc[0] = 0x04;
                toc[1] = 0x02;
                toc[2] = 0xF2;
                toc[4] = 0x86;
                toc[5] = 0x72;
                toc[17] = 0x03;
                let last = end - 1 + 0x30000;
                toc[20..24].copy_from_slice(&last.to_be_bytes());
                toc
            }
            DiscKind::Cd => {
                let mut toc = vec![0u8; 1024];
                // First and last track numbers, the lead-out (BCD MSF).
                let lead_out = texel_disc::Msf::from_lba(end);
                toc[0] = 0x41;
                toc[2] = 0xA0;
                toc[7] = 0x01;
                toc[12] = 0x41;
                toc[14] = 0xA1;
                toc[19] = 0x01;
                toc[24] = 0x41;
                toc[26] = 0xA2;
                toc[27] = lead_out.m_bcd();
                toc[28] = lead_out.s_bcd();
                toc[29] = lead_out.f_bcd();
                toc
            }
        }
    }

    /// Block `lsn` of a read in blocks of `block` bytes.
    fn read_block(&mut self, lsn: u32, block: usize) -> Vec<u8> {
        let mut out = vec![0u8; block];
        let Some(disc) = &mut self.disc else {
            return out;
        };
        self.sectors_read += 1;
        let mut raw = [0u8; RAW_SECTOR_SIZE];
        let ok = match block {
            2064 => {
                // Layer 0 (+ 0x20), the sector number + 0x30000 (big-endian),
                // then the data.
                let id = (lsn + 0x30000).to_be_bytes();
                out[0] = 0x20;
                out[1..4].copy_from_slice(&id[1..4]);
                let mut data = [0u8; DATA_SECTOR_SIZE];
                let ok = disc.medium.read_data(lsn, &mut data).is_ok();
                out[12..12 + DATA_SECTOR_SIZE].copy_from_slice(&data);
                ok
            }
            DATA_SECTOR_SIZE => {
                let mut data = [0u8; DATA_SECTOR_SIZE];
                let ok = disc.medium.read_data(lsn, &mut data).is_ok();
                out.copy_from_slice(&data);
                ok
            }
            // 2328: past the header and subheader; 2340: past the sync.
            _ => {
                let ok = disc.medium.read_sector(lsn, &mut raw).is_ok();
                out.copy_from_slice(&raw[RAW_SECTOR_SIZE - block..]);
                ok
            }
        };
        if !ok {
            self.error = 0x01;
        }
        out
    }

    /// Advances the N command under way by `cycles` IOP cycles; the blocks
    /// read go to `blocks`. Returns whether the IOP should get a CDVD
    /// interrupt.
    pub fn advance(&mut self, cycles: u64) -> bool {
        let Some(wait) = self.busy else {
            return false;
        };
        if let Some(mut r) = self.read {
            r.wait = r.wait.saturating_sub(cycles);
            while r.wait == 0 && r.left > 0 {
                let block = self.read_block(r.lsn, r.block);
                self.blocks.push_back(block);
                r.lsn += 1;
                r.left -= 1;
                self.lsn = r.lsn;
                r.wait = if r.left > 0 { self.block_time(r.block) } else { 0 };
            }
            // The read ends once DMA has taken every block: the drive's
            // buffer cannot get ahead of it (a driver reading in chunks,
            // its DMA set up again after each, sees the end otherwise
            // before its data, and fails the read).
            if r.left > 0 || !self.blocks.is_empty() {
                self.read = Some(r);
                return false;
            }
            self.read = None;
            self.busy = None;
            self.set_drive(drive::PAUSED);
            self.intr_stat |= INTR_DATA_READY | INTR_COMMAND_COMPLETE;
            return true;
        }
        if wait > cycles {
            self.busy = Some(wait - cycles);
            return false;
        }
        self.busy = None;
        if self.drive == drive::SEEKING {
            self.set_drive(drive::PAUSED);
        }
        self.intr_stat |= INTR_COMMAND_COMPLETE;
        true
    }

    fn s_command(&mut self) {
        self.s_result.clear();
        let p = |k: usize| self.s_params.get(k).copied().unwrap_or(0);
        let mut result = vec![0u8; result_size(self.s_command)];
        let known = match self.s_command {
            // Read and write an EEPROM word (bytes swapped).
            0x0A => {
                let at = (usize::from(p(0)) << 8 | usize::from(p(1))) * 2;
                if at + 1 < nvm::SIZE {
                    result[1] = self.nvm[at + 1];
                    result[2] = self.nvm[at];
                }
                true
            }
            0x0B => {
                let at = (usize::from(p(0)) << 8 | usize::from(p(1))) * 2;
                if at + 1 < nvm::SIZE {
                    self.nvm[at + 1] = p(2);
                    self.nvm[at] = p(3);
                }
                true
            }
            // The iLink ID, the model number ("SCPH-...", here none), the MAC.
            0x12 => {
                result[1..9].copy_from_slice(&self.nvm[nvm::ILINK_ID..nvm::ILINK_ID + 8]);
                true
            }
            0x17 => {
                result[1..9].copy_from_slice(&self.nvm[nvm::MODEL_NUMBER..nvm::MODEL_NUMBER + 8]);
                true
            }
            0x37 => {
                result[1..9].copy_from_slice(&self.nvm[nvm::MAC..nvm::MAC + 8]);
                true
            }
            // UpdateStickyFlags: the sticky status starts again from the
            // drive's.
            0x05 => {
                self.sticky = self.drive;
                true
            }
            // The system configuration: open, read and write 16-byte lines,
            // close.
            0x40 => {
                self.config = (p(0) != 0, p(1), p(2), 0);
                true
            }
            0x41 => {
                if self.config.0 {
                    result[0] = 0x80;
                } else if let Some(at) = self.config_line() {
                    result.copy_from_slice(&self.nvm[at..at + 16]);
                    self.config.3 += 1;
                }
                true
            }
            0x42 => {
                if self.config.0
                    && let Some(at) = self.config_line()
                {
                    let line: Vec<u8> = (0..16).map(p).collect();
                    self.nvm[at..at + 16].copy_from_slice(&line);
                    self.config.3 += 1;
                }
                true
            }
            0x43 => {
                self.config = (false, 0, 0, 0);
                true
            }
            _ => false,
        };
        if known {
            self.s_result.extend(result);
            return;
        }
        let result: Vec<u8> = match self.s_command {
            // Subcommands: 0x00, the MechaCon's version.
            0x03 => match self.s_params.first() {
                Some(0x00) => vec![0x03, 0x06, 0x02, 0x00],
                sub => {
                    *self
                        .unknown_s
                        .entry(0x0300 | u16::from(sub.copied().unwrap_or(0)))
                        .or_insert(0) += 1;
                    vec![0]
                }
            },
            // Read the RTC: status, then seconds to year, in BCD.
            0x08 => {
                let s = self.rtc_seconds;
                let days = s / 86_400;
                // A simple calendar from 2000-01-01: months of 30 days are
                // enough for a clock nobody sets.
                vec![
                    0,
                    bcd(s % 60),
                    bcd((s / 60) % 60),
                    bcd((s / 3600) % 24),
                    0,
                    bcd(days % 30 + 1),
                    bcd((days / 30) % 12 + 1),
                    bcd(days / 360),
                ]
            }
            // The region parameters: the zone as a bit, then the EEPROM's
            // eight bytes (15 bytes in all).
            0x36 => {
                result[1] = 1 << self.region.zone;
                result[3..11].copy_from_slice(&self.nvm[nvm::REGION_PARAMS..nvm::REGION_PARAMS + 8]);
                result
            }
            // Commands that only answer their status (and zeros): writing
            // the RTC, forbidding DVD playback, the wake-up time...
            other => {
                if !matches!(other, 0x09 | 0x15 | 0x16 | 0x1B | 0x1C | 0x22 | 0x24 | 0x29) {
                    *self.unknown_s.entry(u16::from(other)).or_insert(0) += 1;
                }
                result
            }
        };
        self.s_result.extend(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mechacon_version_and_clock() {
        let mut c = Cdvd::default();
        c.write(0x17, 0x00);
        c.write(0x16, 0x03);
        assert_eq!(c.read(0x17), 0);
        let v: Vec<u8> = (0..4).map(|_| c.read(0x18)).collect();
        assert_eq!(v, [3, 6, 2, 0]);
        assert_eq!(c.read(0x17), S_RESULT_EMPTY);
        c.rtc_seconds = 3600 + 61;
        c.write(0x16, 0x08);
        let v: Vec<u8> = (0..8).map(|_| c.read(0x18)).collect();
        assert_eq!(v, [0, 0x01, 0x01, 0x01, 0, 0x01, 0x01, 0x00]);
    }

    #[test]
    fn region_parameters() {
        let mut c = Cdvd::new(ConsoleRegion::for_bios(crate::bios::Region::Europe));
        c.write(0x16, 0x36);
        let v: Vec<u8> = (0..15).map(|_| c.read(0x18)).collect();
        assert_eq!(&v[..3], [0, 4, 0]);
        assert_eq!(&v[3..10], b"EEengEE");
        assert_eq!(c.read(0x17), S_RESULT_EMPTY);
    }

    #[test]
    fn config_and_eeprom() {
        let mut c = Cdvd::new(ConsoleRegion::for_bios(crate::bios::Region::Europe));
        let s = |c: &mut Cdvd, cmd: u8, params: &[u8], n: usize| -> Vec<u8> {
            for &b in params {
                c.write(0x17, b);
            }
            c.write(0x16, cmd);
            (0..n).map(|_| c.read(0x18)).collect()
        };
        // Config block 1, line 1: the language defaults.
        s(&mut c, 0x40, &[0, 1, 2], 1);
        s(&mut c, 0x41, &[], 16);
        let line = s(&mut c, 0x41, &[], 16);
        assert_eq!(line[..2], [0x30, 0x21]);
        s(&mut c, 0x43, &[], 1);
        // Write a word, read it back.
        s(&mut c, 0x0B, &[0, 4, 0x12, 0x34], 1);
        assert_eq!(s(&mut c, 0x0A, &[0, 4], 3), [0, 0x12, 0x34]);
        // Commands answer as many bytes as expected.
        assert_eq!(s(&mut c, 0x22, &[], 10), [0; 10]);
        assert_eq!(c.read(0x17), S_RESULT_EMPTY);
    }

    #[test]
    fn n_commands_complete_with_an_interrupt() {
        let mut c = Cdvd::default();
        assert!(!c.write(0x04, 0x00));
        assert_eq!(c.read(0x05), N_BUSY);
        assert!(c.advance(ms(2)));
        assert_eq!(c.read(0x05), N_READY);
        assert_eq!(c.read(0x08), INTR_COMMAND_COMPLETE);
        c.write(0x08, INTR_COMMAND_COMPLETE);
        assert_eq!(c.read(0x08), 0);
    }

    /// A disc of `sectors` sectors, each filled with its number's low byte.
    fn disc(sectors: u32, kind: DiscKind) -> Inserted {
        struct Pattern;
        impl FileReader for Pattern {
            fn read_at(&mut self, _: usize, offset: u64, buf: &mut [u8]) -> Result<(), texel_disc::ReadError> {
                buf.fill((offset / DATA_SECTOR_SIZE as u64) as u8);
                Ok(())
            }
        }
        let layout = texel_disc::Layout::single_file("disc.iso", u64::from(sectors) * 2048).expect("a layout");
        Inserted {
            medium: Disc::new(layout, Box::new(Pattern)),
            kind,
        }
    }

    #[test]
    fn dvd_reads_take_time_and_fill_blocks() {
        let mut c = Cdvd::default();
        c.insert(Some(disc(100, DiscKind::Dvd)));
        assert_eq!(c.read(0x0F), 0x14);
        // ReadDvd: sector 5, 3 sectors.
        for b in [5, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0] {
            c.write(0x05, b);
        }
        c.write(0x04, 0x08);
        assert_eq!(c.read(0x0A), drive::READING);
        // Stopped: it spins up first (a third of a second).
        assert!(!c.advance(ms(300)));
        assert!(c.blocks.is_empty());
        // Not done while a block waits for DMA.
        for _ in 0..100 {
            assert!(!c.advance(ms(1)));
        }
        assert_eq!(c.blocks.len(), 3);
        assert_eq!(c.read(0x0A), drive::READING);
        // DMA takes them: then it is.
        let b: Vec<_> = c.blocks.drain(..).collect();
        assert!(c.advance(ms(1)));
        assert_eq!(c.read(0x08), INTR_DATA_READY | INTR_COMMAND_COMPLETE);
        assert_eq!(c.read(0x0A), drive::PAUSED);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].len(), 2064);
        assert_eq!(b[0][..4], [0x20, 0x03, 0x00, 0x05]);
        assert_eq!(b[2][12], 7);
    }
}
