//! CD-ROM controller: command/response protocol, sector reads, CD-DA
//! playback and XA-ADPCM decoding.

use std::collections::VecDeque;

use texel_disc::{Disc, FileReader, Msf, RAW_SECTOR_SIZE};

use crate::irq::{Interrupt, Irq};
use crate::spu::Spu;

pub type DiscImage = Disc<Box<dyn FileReader>>;

const CPU_CLOCK: u32 = 33_868_800;
/// Sector period at single speed (75 sectors per second).
const SECTOR_CYCLES: u32 = CPU_CLOCK / 75;
/// Delay of the first response (INT3) to most commands.
const ACK_DELAY: u32 = 25_000;
const SEEK_DELAY: u32 = 100_000;

// Status byte bits.
const STAT_ERROR: u8 = 0x01;
const STAT_MOTOR: u8 = 0x02;
const STAT_SHELL_OPEN: u8 = 0x10;
const STAT_READING: u8 = 0x20;
const STAT_SEEKING: u8 = 0x40;
const STAT_PLAYING: u8 = 0x80;

// Mode bits.
const MODE_CDDA: u8 = 0x01;
const MODE_AUTOPAUSE: u8 = 0x02;
const MODE_REPORT: u8 = 0x04;
const MODE_XA_FILTER: u8 = 0x08;
const MODE_WHOLE_SECTOR: u8 = 0x20;
const MODE_XA_ADPCM: u8 = 0x40;
const MODE_DOUBLE_SPEED: u8 = 0x80;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
enum State {
    Idle,
    Seeking { then: AfterSeek },
    Reading,
    Playing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
enum AfterSeek {
    Read,
    Play,
    /// SeekL/SeekP: answer with INT2.
    Complete,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Event {
    delay: u32,
    code: u8,
    response: Vec<u8>,
}

/// XA-ADPCM decoder state for one channel, and the 37.8/18.9 kHz to 44.1 kHz
/// resampler.
#[derive(Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct XaChannel {
    old: i32,
    older: i32,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct CdRom {
    /// Not in save states: the disc stays in the drive.
    #[serde(skip)]
    pub disc: Option<DiscImage>,
    /// Region letter reported by GetID ('A', 'E' or 'I'); 0: the disc's own,
    /// from its licence sector (region-free BIOS).
    pub region: u8,
    index: u8,
    params: VecDeque<u8>,
    response: VecDeque<u8>,
    data: Vec<u8>,
    data_pos: usize,
    sector: Box<[u8]>,
    sector_valid: bool,
    ie: u8,
    flags: u8,
    events: VecDeque<Event>,
    busy: bool,
    motor: bool,
    mode: u8,
    setloc: u32,
    setloc_pending: bool,
    pos: u32,
    state: State,
    timer: u32,
    filter_file: u8,
    filter_channel: u8,
    muted: bool,
    last_header: [u8; 8],
    lid_open: bool,
    /// Latched when the lid opens, until Getstat with the lid closed.
    shell_was_opened: bool,
    /// Track being played, for auto-pause at its end.
    play_track: Option<u8>,
    xa: [XaChannel; 2],
    xa_phase: u32,
    xa_prev: (i16, i16),
    /// Audio volumes: [L->L, L->R, R->L, R->R], and the pending ones.
    volume: [u8; 4],
    volume_pending: [u8; 4],
    /// Diagnostics, when set: the commands sent (with their parameters),
    /// and the XA sectors played and skipped (filtered out) since.
    #[serde(skip)]
    pub log: Option<Vec<String>>,
    #[serde(skip)]
    xa_counts: (u32, u32),
}

impl Default for CdRom {
    fn default() -> Self {
        Self::new()
    }
}

impl CdRom {
    /// Rollback (see `Bus::adopt`): the disc, and diagnostics.
    pub(crate) fn adopt(&mut self, old: &mut CdRom) {
        self.disc = old.disc.take();
        self.log = old.log.take();
        self.xa_counts = old.xa_counts;
    }

    pub fn new() -> Self {
        Self {
            disc: None,
            region: b'A',
            index: 0,
            params: VecDeque::new(),
            response: VecDeque::new(),
            data: Vec::new(),
            data_pos: 0,
            sector: vec![0; RAW_SECTOR_SIZE].into_boxed_slice(),
            sector_valid: false,
            ie: 0,
            flags: 0,
            events: VecDeque::new(),
            busy: false,
            motor: false,
            mode: 0,
            setloc: 0,
            setloc_pending: false,
            pos: 0,
            state: State::Idle,
            timer: 0,
            filter_file: 0,
            filter_channel: 0,
            muted: false,
            last_header: [0; 8],
            play_track: None,
            lid_open: false,
            shell_was_opened: false,
            xa: [XaChannel::default(); 2],
            xa_phase: 0,
            xa_prev: (0, 0),
            volume: [0x80, 0, 0, 0x80],
            volume_pending: [0x80, 0, 0, 0x80],
            log: None,
            xa_counts: (0, 0),
        }
    }

    pub fn debug(&self) -> String {
        format!(
            "cd: state={:?} mode={:02x} pos={} flags={:02x} ie={:02x} events={} data={}/{} filter={}/{}",
            self.state,
            self.mode,
            self.pos,
            self.flags,
            self.ie,
            self.events.len(),
            self.data_pos,
            self.data.len(),
            self.filter_file,
            self.filter_channel
        )
    }

    /// Closes the lid on `disc` (or on nothing).
    pub fn insert_disc(&mut self, disc: Option<DiscImage>) {
        self.disc = disc;
        self.lid_open = false;
        self.state = State::Idle;
        self.motor = self.disc.is_some();
        self.drop_data_events();
    }

    /// Opens the lid: the disc stops, reads fail, and the status reports
    /// the open shell until the game reads it once the lid is closed again.
    pub fn open_lid(&mut self) {
        if self.lid_open {
            return;
        }
        let was_busy = self.state != State::Idle;
        self.lid_open = true;
        self.shell_was_opened = true;
        self.disc = None;
        self.motor = false;
        self.state = State::Idle;
        self.drop_data_events();
        if was_busy {
            let s = self.stat() | STAT_ERROR;
            self.queue(0, 5, vec![s, 0x08]);
        }
    }

    pub fn lid_open(&self) -> bool {
        self.lid_open
    }

    fn stat(&self) -> u8 {
        let mut s = 0;
        if self.lid_open || self.shell_was_opened {
            s |= STAT_SHELL_OPEN;
        }
        if self.disc.is_none() {
            return s | STAT_SHELL_OPEN;
        }
        if self.motor {
            s |= STAT_MOTOR;
        }
        s |= match self.state {
            State::Idle => 0,
            State::Seeking { .. } => STAT_SEEKING,
            State::Reading => STAT_READING,
            State::Playing => STAT_PLAYING,
        };
        s
    }

    pub fn read(&mut self, port: u32) -> u8 {
        match (port, self.index) {
            (0, _) => {
                let mut s = self.index;
                s |= u8::from(self.params.is_empty()) << 3;
                s |= u8::from(self.params.len() < 16) << 4;
                s |= u8::from(!self.response.is_empty()) << 5;
                s |= u8::from(self.data_pos < self.data.len()) << 6;
                s |= u8::from(self.busy) << 7;
                s
            }
            (1, _) => self.response.pop_front().unwrap_or(0),
            (2, _) => self.read_data_byte(),
            (3, 0 | 2) => self.ie | 0xe0,
            (3, _) => self.flags | 0xe0,
            _ => 0,
        }
    }

    pub fn write(&mut self, port: u32, v: u8) {
        match (port, self.index) {
            (0, _) => self.index = v & 3,
            (1, 0) => self.command(v),
            (2, 0) => {
                if self.params.len() < 16 {
                    self.params.push_back(v);
                }
            }
            (2, 1) => self.ie = v & 0x1f,
            (3, 0) => {
                if v & 0x80 != 0 {
                    if self.sector_valid && self.data_pos >= self.data.len() {
                        let (start, len) = if self.mode & MODE_WHOLE_SECTOR != 0 {
                            (12, 0x924)
                        } else {
                            (24, 0x800)
                        };
                        self.data.clear();
                        self.data.extend_from_slice(&self.sector[start..start + len]);
                        self.data_pos = 0;
                    }
                } else {
                    self.data.clear();
                    self.data_pos = 0;
                }
            }
            (3, 1) => {
                self.flags &= !(v & 0x1f);
                if v & 0x40 != 0 {
                    self.params.clear();
                }
            }
            (2, 2) => self.volume_pending[0] = v,
            (3, 2) => self.volume_pending[1] = v,
            (1, 3) => self.volume_pending[3] = v,
            (2, 3) => self.volume_pending[2] = v,
            (3, 3) if v & 0x20 != 0 => {
                self.volume = self.volume_pending;
            }
            _ => {}
        }
    }

    pub fn data_ready(&self) -> bool {
        self.data_pos < self.data.len()
    }

    fn read_data_byte(&mut self) -> u8 {
        let b = self.data.get(self.data_pos).copied().unwrap_or(0);
        if self.data_pos < self.data.len() {
            self.data_pos += 1;
        }
        b
    }

    pub fn read_data_word(&mut self) -> u32 {
        u32::from_le_bytes([
            self.read_data_byte(),
            self.read_data_byte(),
            self.read_data_byte(),
            self.read_data_byte(),
        ])
    }

    fn queue(&mut self, delay: u32, code: u8, response: Vec<u8>) {
        self.events.push_back(Event { delay, code, response });
    }

    fn ack(&mut self) {
        let s = self.stat();
        self.queue(ACK_DELAY, 3, vec![s]);
    }

    fn error(&mut self, code: u8) {
        let s = self.stat() | STAT_ERROR;
        self.queue(ACK_DELAY, 5, vec![s, code]);
    }

    fn param(&mut self) -> u8 {
        self.params.pop_front().unwrap_or(0)
    }

    fn sector_cycles(&self) -> u32 {
        if self.mode & MODE_DOUBLE_SPEED != 0 {
            SECTOR_CYCLES / 2
        } else {
            SECTOR_CYCLES
        }
    }

    /// The log so far (see `log`), with the XA sectors since the last
    /// line; logging goes on.
    pub fn take_log(&mut self) -> Vec<String> {
        self.log_xa();
        self.log.replace(Vec::new()).unwrap_or_default()
    }

    fn log_xa(&mut self) {
        if let Some(log) = self.log.as_mut() {
            let (played, skipped) = std::mem::take(&mut self.xa_counts);
            if played + skipped > 0 {
                log.push(format!(
                    "  (XA sectors: {played} played, {skipped} filtered out; muted {}, volume {:?})",
                    self.muted, self.volume
                ));
            }
        }
    }

    fn command(&mut self, cmd: u8) {
        self.log_xa();
        if let Some(log) = self.log.as_mut() {
            log.push(format!("{cmd:02x} {:02x?} (mode {:02x})", self.params, self.mode));
        }
        self.response.clear();
        self.busy = true;
        let needs_disc = matches!(cmd, 0x03..=0x06 | 0x15 | 0x16 | 0x1b);
        if needs_disc && self.disc.is_none() {
            self.params.clear();
            self.error(0x80);
            return;
        }
        match cmd {
            0x01 => {
                self.ack();
                // The "shell opened" bit clears once read with the lid closed.
                if !self.lid_open {
                    self.shell_was_opened = false;
                }
            }
            0x02 => {
                let (m, s, f) = (self.param(), self.param(), self.param());
                self.setloc = Msf::from_bcd(m, s, f).to_lba();
                self.setloc_pending = true;
                self.ack();
            }
            0x03 => {
                let track = self.param();
                if track != 0 {
                    let n = texel_disc::Msf::from_bcd(0, 0, track).f;
                    if let Some(t) = self.disc.as_ref().and_then(|d| d.layout.track(n)) {
                        self.setloc = t.start;
                        self.setloc_pending = true;
                    }
                }
                self.ack();
                self.start_seek(AfterSeek::Play);
            }
            0x04 | 0x05 => self.ack(),
            0x06 | 0x1b => {
                self.ack();
                self.start_seek(AfterSeek::Read);
            }
            0x07 => {
                self.motor = true;
                self.ack();
                let s = self.stat();
                self.queue(ACK_DELAY * 4, 2, vec![s]);
            }
            0x08 => {
                self.ack();
                self.state = State::Idle;
                self.motor = false;
                let s = self.stat();
                self.queue(ACK_DELAY * 8, 2, vec![s]);
            }
            0x09 => {
                self.ack();
                // The second response comes faster when already paused.
                let delay = if self.state == State::Idle {
                    7_000
                } else {
                    2_000_000 / 3
                };
                self.state = State::Idle;
                self.drop_data_events();
                let s = self.stat();
                self.queue(delay, 2, vec![s]);
            }
            0x0a => {
                self.mode = 0;
                self.motor = self.disc.is_some();
                self.state = State::Idle;
                self.drop_data_events();
                self.ack();
                let s = self.stat();
                self.queue(ACK_DELAY * 3, 2, vec![s]);
            }
            0x0b => {
                self.muted = true;
                self.ack();
            }
            0x0c => {
                self.muted = false;
                self.ack();
            }
            0x0d => {
                self.filter_file = self.param();
                self.filter_channel = self.param();
                self.ack();
            }
            0x0e => {
                self.mode = self.param();
                self.ack();
            }
            0x0f => {
                let s = self.stat();
                let r = vec![s, self.mode, 0, self.filter_file, self.filter_channel];
                self.queue(ACK_DELAY, 3, r);
            }
            0x10 => {
                let r = self.last_header.to_vec();
                self.queue(ACK_DELAY, 3, r);
            }
            0x11 => {
                let r = self.position_report();
                self.queue(ACK_DELAY, 3, r);
            }
            0x12 => {
                self.param();
                self.ack();
                let s = self.stat();
                self.queue(ACK_DELAY * 4, 2, vec![s]);
            }
            0x13 => {
                let last = self
                    .disc
                    .as_ref()
                    .and_then(|d| d.layout.tracks.last())
                    .map_or(1, |t| t.number);
                let s = self.stat();
                self.queue(ACK_DELAY, 3, vec![s, 0x01, to_bcd(last)]);
            }
            0x14 => {
                let n = from_bcd(self.param());
                let lba = match &self.disc {
                    Some(d) if n == 0 => Some(d.layout.lead_out()),
                    Some(d) => d.layout.track(n).map(|t| t.start),
                    None => None,
                };
                match lba {
                    Some(lba) => {
                        let msf = Msf::from_lba(lba);
                        let s = self.stat();
                        self.queue(ACK_DELAY, 3, vec![s, msf.m_bcd(), msf.s_bcd()]);
                    }
                    None => self.error(0x10),
                }
            }
            0x15 | 0x16 => {
                self.ack();
                self.start_seek(AfterSeek::Complete);
            }
            0x19 => match self.param() {
                0x20 => self.queue(ACK_DELAY, 3, vec![0x94, 0x09, 0x19, 0xc0]),
                _ => self.error(0x10),
            },
            0x1a => {
                if self.disc.is_some() {
                    self.ack();
                    let letter = if self.region == 0 {
                        self.disc_region()
                    } else {
                        self.region
                    };
                    let r = vec![0x02, 0x00, 0x20, 0x00, b'S', b'C', b'E', letter];
                    self.queue(ACK_DELAY * 2, 2, r);
                } else {
                    self.ack();
                    self.queue(ACK_DELAY * 2, 5, vec![0x08, 0x40, 0, 0, 0, 0, 0, 0]);
                }
            }
            0x1e => {
                self.ack();
                let s = self.stat();
                self.queue(CPU_CLOCK / 2, 2, vec![s]);
            }
            _ => self.error(0x40),
        }
        self.params.clear();
    }

    /// The region of the disc, from the licence text of sector 4:
    /// "Licensed by Sony Computer Entertainment Euro pe" (or "Amer ica",
    /// or "Inc." for Japan).
    fn disc_region(&mut self) -> u8 {
        let mut data = [0u8; texel_disc::DATA_SECTOR_SIZE];
        let Some(disc) = self.disc.as_mut() else { return b'A' };
        if disc.read_data(4, &mut data).is_err() {
            return b'A';
        }
        let text: Vec<u8> = data.iter().copied().filter(u8::is_ascii_alphabetic).collect();
        let has = |needle: &[u8]| text.windows(needle.len()).any(|w| w == needle);
        if has(b"Europe") {
            b'E'
        } else if has(b"America") {
            b'A'
        } else {
            b'I'
        }
    }

    fn drop_data_events(&mut self) {
        // Keep the events of the current command (the first one is its ack).
        self.events.retain(|e| e.code != 1 && e.code != 4);
    }

    fn start_seek(&mut self, then: AfterSeek) {
        self.drop_data_events();
        self.motor = true;
        if self.setloc_pending {
            self.pos = self.setloc;
            self.setloc_pending = false;
        }
        self.state = State::Seeking { then };
        self.timer = SEEK_DELAY;
        self.play_track = None;
    }

    /// GetlocP: the subchannel Q of the last sector read: its position,
    /// but for the sectors a protected disc's SBI file lists (LibCrypt
    /// checks that they differ).
    fn position_report(&self) -> Vec<u8> {
        let Some(disc) = &self.disc else { return vec![0; 8] };
        let q = disc.sub_q(self.pos.saturating_sub(1));
        // Track, index, relative MSF, absolute MSF (byte 6 is always zero).
        vec![q[1], q[2], q[3], q[4], q[5], q[7], q[8], q[9]]
    }

    /// The CPU cycles until the drive does something: delivers its next
    /// response (once the last is acknowledged), or the next sector.
    pub fn next_event(&self) -> Option<u32> {
        let response = self
            .events
            .front()
            .filter(|_| self.flags & 0x1f == 0)
            .map(|e| e.delay.max(1));
        let sector = (self.state != State::Idle).then_some(self.timer.max(1));
        match (response, sector) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn tick(&mut self, cycles: u32, irq: &mut Irq, spu: &mut Spu) {
        // Deliver the next response once the previous one is acknowledged.
        if let Some(e) = self.events.front_mut() {
            e.delay = e.delay.saturating_sub(cycles);
            if e.delay == 0 && self.flags & 0x1f == 0 {
                let e = self.events.pop_front().expect("an event");
                self.flags = e.code;
                self.response = e.response.into();
                if e.code == 3 || e.code == 5 {
                    self.busy = false;
                }
                if self.ie & self.flags != 0 {
                    irq.raise(Interrupt::CdRom);
                }
            }
        }

        if self.state == State::Idle {
            return;
        }
        self.timer = self.timer.saturating_sub(cycles);
        if self.timer > 0 {
            return;
        }
        match self.state {
            State::Seeking { then } => {
                self.state = match then {
                    AfterSeek::Read => State::Reading,
                    AfterSeek::Play => State::Playing,
                    AfterSeek::Complete => {
                        let s = self.stat() & !STAT_SEEKING;
                        self.queue(0, 2, vec![s]);
                        State::Idle
                    }
                };
                self.timer = self.sector_cycles();
            }
            State::Reading => {
                self.timer = self.sector_cycles();
                self.read_sector(spu);
            }
            State::Playing => {
                self.timer = self.sector_cycles();
                self.play_sector(spu);
            }
            State::Idle => {}
        }
    }

    fn load_sector(&mut self) -> Option<texel_disc::SectorInfo> {
        let disc = self.disc.as_mut()?;
        let sector: &mut [u8; RAW_SECTOR_SIZE] = (&mut *self.sector).try_into().ok()?;
        let info = disc.read_sector(self.pos, sector).ok();
        self.pos += 1;
        info
    }

    fn read_sector(&mut self, spu: &mut Spu) {
        let Some(info) = self.load_sector() else {
            self.state = State::Idle;
            let s = self.stat() | STAT_ERROR;
            self.queue(0, 5, vec![s, 0x04]);
            return;
        };
        if info.audio && self.mode & MODE_CDDA == 0 {
            // Reading audio as data is an error unless CDDA mode is set.
            self.state = State::Idle;
            let s = self.stat() | STAT_ERROR;
            self.queue(0, 5, vec![s, 0x04]);
            return;
        }
        self.last_header.copy_from_slice(&self.sector[12..20]);
        let (file, channel, submode, coding) = (self.sector[16], self.sector[17], self.sector[18], self.sector[19]);
        let is_xa_audio = self.sector[15] == 2 && submode & 0x04 != 0 && submode & 0x20 != 0;
        if self.mode & MODE_XA_ADPCM != 0 && is_xa_audio {
            let matches = file == self.filter_file && channel == self.filter_channel;
            if self.mode & MODE_XA_FILTER == 0 || matches {
                self.xa_counts.0 += 1;
                let peak = self.decode_xa(coding, spu);
                if let Some(log) = self.log.as_mut() {
                    log.push(format!(
                        "  XA at {}: file {file} channel {channel} submode {submode:02x} coding {coding:02x}, peak {peak}",
                        self.pos - 1
                    ));
                }
            } else {
                self.xa_counts.1 += 1;
            }
            return;
        }
        if self.mode & MODE_XA_FILTER != 0 && is_xa_audio {
            return;
        }
        self.sector_valid = true;
        if let Some(log) = self.log.as_mut() {
            log.push(format!(
                "  data at {}: file {file} channel {channel} submode {submode:02x}{}",
                self.pos - 1,
                if self.events.iter().any(|e| e.code == 1) {
                    " (an INT1 still pending: none raised)"
                } else {
                    ""
                }
            ));
        }
        // Do not pile up data interrupts the game has not handled yet.
        if !self.events.iter().any(|e| e.code == 1) {
            let s = self.stat();
            self.queue(0, 1, vec![s]);
        }
    }

    fn play_sector(&mut self, spu: &mut Spu) {
        let lba = self.pos;
        let Some(info) = self.load_sector() else {
            self.state = State::Idle;
            let s = self.stat();
            self.queue(0, 4, vec![s]);
            return;
        };
        let track = *self.play_track.get_or_insert(info.track);
        if self.mode & MODE_AUTOPAUSE != 0 && info.track != track {
            self.state = State::Idle;
            let s = self.stat();
            self.queue(0, 4, vec![s]);
            return;
        }
        if info.audio && !self.muted {
            for frame in self.sector.as_chunks::<4>().0 {
                let l = i16::from_le_bytes([frame[0], frame[1]]);
                let r = i16::from_le_bytes([frame[2], frame[3]]);
                self.push_audio(spu, l, r);
            }
        }
        // Reports alternate absolute (sector 0 mod 10) and relative
        // (sector 5 mod 10, seconds with bit 7 set) positions.
        let abs = Msf::from_lba(lba);
        if self.mode & MODE_REPORT != 0 && abs.f.is_multiple_of(5) && !self.events.iter().any(|e| e.code == 1) {
            let start = self
                .disc
                .as_ref()
                .and_then(|d| d.layout.track(info.track))
                .map_or(0, |t| t.start);
            let index = u8::from(lba >= start);
            let (m, s, f) = if abs.f.is_multiple_of(10) {
                (abs.m_bcd(), abs.s_bcd(), abs.f_bcd())
            } else {
                let rel = Msf::from_sectors(lba.abs_diff(start));
                (rel.m_bcd(), rel.s_bcd() | 0x80, rel.f_bcd())
            };
            let r = vec![self.stat(), to_bcd(info.track), index, m, s, f, 0, 0];
            self.queue(0, 1, r);
        }
    }

    fn push_audio(&self, spu: &mut Spu, l: i16, r: i16) {
        let v = &self.volume;
        let mix = |a: i16, va: u8, b: i16, vb: u8| -> i16 {
            ((i32::from(a) * i32::from(va) + i32::from(b) * i32::from(vb)) >> 7).clamp(-0x8000, 0x7fff) as i16
        };
        spu.push_cd_audio(mix(l, v[0], r, v[2]), mix(l, v[1], r, v[3]));
    }

    /// Decodes and plays an XA sector: returns its peak (diagnostics).
    fn decode_xa(&mut self, coding: u8, spu: &mut Spu) -> u16 {
        let stereo = coding & 1 != 0;
        let half_rate = coding & 4 != 0;
        let eight_bit = coding & 0x10 != 0;
        let units = if eight_bit { 4 } else { 8 };
        let mut left = Vec::with_capacity(4032);
        let mut right = Vec::with_capacity(2016);
        for group in 0..18 {
            let base = 24 + group * 128;
            let g = &self.sector[base..base + 128];
            for unit in 0..units {
                let header = g[4 + unit];
                let shift = match header & 0xf {
                    s @ 0..=12 => s,
                    _ => 9,
                };
                let filter = usize::from((header >> 4) & 3);
                let ch = if stereo { unit & 1 } else { 0 };
                let out = if ch == 0 { &mut left } else { &mut right };
                let st = &mut self.xa[ch];
                for j in 0..28 {
                    let raw = if eight_bit {
                        i32::from((g[16 + j * 4 + unit] as i8 as i16) << 8)
                    } else {
                        let byte = g[16 + j * 4 + unit / 2];
                        let nibble = (byte >> ((unit & 1) * 4)) & 0xf;
                        ((u16::from(nibble) << 12) as i16) as i32
                    };
                    let t = raw >> shift;
                    const POS: [i32; 4] = [0, 60, 115, 98];
                    const NEG: [i32; 4] = [0, 0, -52, -55];
                    let s = (t + ((st.old * POS[filter] + st.older * NEG[filter] + 32) >> 6)).clamp(-0x8000, 0x7fff);
                    st.older = st.old;
                    st.old = s;
                    out.push(s as i16);
                }
            }
        }
        if !stereo {
            right.clone_from(&left);
        }
        let peak = left.iter().chain(&right).map(|s| s.unsigned_abs()).max().unwrap_or(0);
        if self.muted {
            return peak;
        }
        // Linear resampling to 44.1 kHz.
        let in_rate = if half_rate { 18_900 } else { 37_800 };
        let mut prev = self.xa_prev;
        for (&l, &r) in left.iter().zip(right.iter()) {
            while self.xa_phase < 44_100 {
                let t = self.xa_phase as i32;
                let lerp = |a: i16, b: i16| (i32::from(a) + (i32::from(b) - i32::from(a)) * t / 44_100) as i16;
                self.push_audio(spu, lerp(prev.0, l), lerp(prev.1, r));
                self.xa_phase += in_rate;
            }
            self.xa_phase -= 44_100;
            prev = (l, r);
        }
        self.xa_prev = prev;
        peak
    }
}

fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0xf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use texel_disc::{FileReader, Layout, ReadError};

    struct Iso(Vec<u8>);

    impl FileReader for Iso {
        fn read_at(&mut self, _: usize, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
            let o = offset as usize;
            buf.copy_from_slice(&self.0[o..o + buf.len()]);
            Ok(())
        }
    }

    fn with_licence(text: &[u8]) -> u8 {
        let mut iso = vec![0u8; 2048 * 8];
        iso[4 * 2048..4 * 2048 + text.len()].copy_from_slice(text);
        let layout = Layout::single_file("x.iso", iso.len() as u64).unwrap();
        let mut cd = CdRom::new();
        cd.insert_disc(Some(Disc::new(layout, Box::new(Iso(iso)) as Box<dyn FileReader>)));
        cd.disc_region()
    }

    #[test]
    fn disc_region_from_the_licence_sector() {
        assert_eq!(
            with_licence(b"          Licensed  by          Sony Computer Entertainment Euro pe   "),
            b'E'
        );
        assert_eq!(
            with_licence(b"          Licensed  by          Sony Computer Entertainment Amer  ica "),
            b'A'
        );
        assert_eq!(
            with_licence(b"          Licensed  by          Sony Computer Entertainment Inc."),
            b'I'
        );
    }
}
