//! SIO2: the IOP's interface to the controller ports and memory cards. A
//! transfer is a list of commands (SEND3), each with its port's settings
//! (SEND1/SEND2) and bytes to send from the FIFO; started by CTRL bit 0, it
//! leaves the replies in the output FIFO and its status in RECV1-3.
//!
//! Controllers: DualShocks (`texel-ps1`'s), on ports 0 and 1, whose commands
//! start with 0x01; memory cards on ports 2 and 3, whose commands start
//! with 0x81. No multitaps (0x21): they answer "no device".

use std::collections::VecDeque;

use crate::dualshock2::DualShock2;

use crate::memcard::MemoryCard;

/// RECV1 when a device answered, and when none did.
const RECV1_DEVICE: u32 = 0x1100;
const RECV1_NO_DEVICE: u32 = 0x1D100;
const RECV2: u32 = 0xF;

/// A command, for diagnostics: port, bytes sent and received (counts, then
/// the bytes).
pub type Exchange = (usize, usize, usize, Vec<u8>, Vec<u8>);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Sio2 {
    /// The controllers in ports 0 and 1.
    pub pads: [DualShock2; 2],
    /// The memory cards in ports 2 and 3 (slots 1 and 2).
    #[serde(skip)]
    pub cards: [Option<Box<MemoryCard>>; 2],
    send3: [u32; 16],
    send12: [u32; 8],
    ctrl: u32,
    recv1: u32,
    fifo_in: VecDeque<u8>,
    fifo_out: VecDeque<u8>,
    /// DMA blocks the bytes to send came in (channel 11): each command's
    /// bytes start a block, whose rest is padding. Empty when they came
    /// through the FIFO register.
    in_blocks: VecDeque<usize>,
    /// The commands' reply lengths, for DMA channel 12, which gives each
    /// its own block.
    out_lengths: VecDeque<usize>,
    intr: u32,
    /// A transfer was started, and waits for its bytes (DMA channel 11
    /// often brings them after CTRL starts the transfer).
    started: bool,
    /// A transfer ran: the IOP's SIO2 interrupt is due.
    pub irq: bool,
    /// Other registers (0x84: the remote control bypass, on slim models),
    /// kept as written.
    other: std::collections::BTreeMap<u32, u32>,
    /// Transfers run, for diagnostics.
    pub transfers: u64,
    /// The last commands, for diagnostics: (port, send, receive, bytes sent,
    /// bytes received).
    #[serde(skip)]
    pub log: VecDeque<Exchange>,
    /// The first memory card commands, for diagnostics.
    #[serde(skip)]
    pub card_log: Vec<Exchange>,
    /// Register accesses, for diagnostics: (write, offset, value), the
    /// first ones after transfer 20.
    #[serde(skip)]
    pub trace: Vec<(bool, u32, u32)>,
}

impl Default for Sio2 {
    fn default() -> Self {
        Self {
            pads: [DualShock2::new(true), DualShock2::new(true)],
            cards: [None, None],
            send3: [0; 16],
            send12: [0; 8],
            ctrl: 0,
            recv1: 0,
            fifo_in: VecDeque::new(),
            fifo_out: VecDeque::new(),
            in_blocks: VecDeque::new(),
            out_lengths: VecDeque::new(),
            intr: 0,
            started: false,
            irq: false,
            other: std::collections::BTreeMap::new(),
            transfers: 0,
            log: VecDeque::new(),
            card_log: Vec::new(),
            trace: Vec::new(),
        }
    }
}

impl Sio2 {
    /// Bytes to send (DMA channel 11, or the FIFO register): the started
    /// transfer runs once it has them all.
    pub fn push_in(&mut self, bytes: &[u8]) {
        self.fifo_in.extend(bytes);
        if self.started && self.fifo_in.len() >= self.bytes_to_send() {
            self.run();
        }
    }

    /// DMA channel 11: a block of bytes to send (one command's, padded).
    pub fn push_block(&mut self, block: &[u8]) {
        self.in_blocks.push_back(block.len());
        self.push_in(block);
    }

    /// DMA channel 12: a byte received.
    pub fn pop_out(&mut self) -> u8 {
        self.finish();
        self.fifo_out.pop_front().unwrap_or(0xFF)
    }

    /// Whether replies wait for DMA channel 12 (it waits for them: the
    /// drivers start it before the transfer).
    pub fn has_replies(&self) -> bool {
        !self.out_lengths.is_empty()
    }

    /// DMA channel 12: a block of `size` bytes, the next command's reply
    /// at its start.
    pub fn pop_block(&mut self, size: usize) -> Vec<u8> {
        self.finish();
        let n = self.out_lengths.pop_front().unwrap_or(size).min(size);
        let mut block: Vec<u8> = (0..n).map(|_| self.fifo_out.pop_front().unwrap_or(0xFF)).collect();
        block.resize(size, 0xFF);
        block
    }

    /// The bytes the queued commands send.
    fn bytes_to_send(&self) -> usize {
        self.send3
            .iter()
            .take_while(|&&c| c != 0)
            .map(|c| ((c >> 8) & 0x1FF) as usize)
            .sum()
    }

    /// Runs a started transfer now, whatever arrived.
    fn finish(&mut self) {
        if self.started {
            self.run();
        }
    }

    fn run(&mut self) {
        self.started = false;
        self.transfer();
        self.irq = true;
    }

    /// `offset`: from 0x1F808200.
    pub fn read(&mut self, offset: u32) -> u32 {
        let v = self.read_inner(offset);
        if self.transfers >= 20 && self.trace.len() < 300 {
            self.trace.push((false, offset, v));
        }
        v
    }

    fn read_inner(&mut self, offset: u32) -> u32 {
        match offset {
            0x00..0x40 => self.send3[(offset >> 2) as usize],
            0x40..0x60 => self.send12[((offset - 0x40) >> 2) as usize],
            0x64 => u32::from(self.pop_out()),
            0x68 => self.ctrl,
            0x6C => {
                self.finish();
                self.recv1
            }
            0x70 => RECV2,
            0x74 => 0,
            0x80 => self.intr,
            _ => self.other.get(&offset).copied().unwrap_or(0),
        }
    }

    pub fn write(&mut self, offset: u32, v: u32) {
        if self.transfers >= 20 && self.trace.len() < 300 {
            self.trace.push((true, offset, v));
        }
        match offset {
            0x00..0x40 => self.send3[(offset >> 2) as usize] = v,
            0x40..0x60 => self.send12[((offset - 0x40) >> 2) as usize] = v,
            0x60 => self.push_in(&[v as u8]),
            0x68 => {
                // Bits 2-3: reset the FIFOs (the drivers write them first, then
                // again with the start bit, after queuing the bytes).
                if v & 0xC != 0 && v & 1 == 0 {
                    self.fifo_in.clear();
                    self.fifo_out.clear();
                    self.in_blocks.clear();
                    self.out_lengths.clear();
                }
                self.ctrl = v & !1;
                if v & 1 != 0 {
                    self.started = true;
                    if self.fifo_in.len() >= self.bytes_to_send() {
                        self.run();
                    }
                }
            }
            0x80 => self.intr &= !v,
            _ => {
                self.other.insert(offset, v);
            }
        }
    }

    /// Runs the queued commands: each exchanges its bytes with the device
    /// on its port, if any.
    fn transfer(&mut self) {
        self.transfers += 1;
        // SEND3: port (bits 0-1), bytes to send (8-16) and to receive (18-26).
        let commands: Vec<u32> = self.send3.iter().copied().take_while(|&c| c != 0).collect();
        for cmd in commands {
            let port = (cmd & 3) as usize;
            let send = ((cmd >> 8) & 0x1FF) as usize;
            let receive = ((cmd >> 18) & 0x1FF) as usize;
            let bytes: Vec<u8> = (0..send).map(|_| self.fifo_in.pop_front().unwrap_or(0)).collect();
            // The rest of its DMA block: padding.
            if let Some(block) = self.in_blocks.pop_front() {
                for _ in send..block {
                    self.fifo_in.pop_front();
                }
            }
            self.out_lengths.push_back(receive);
            let out_before = self.fifo_out.len();
            let card = (port >= 2 && bytes.first() == Some(&0x81))
                .then(|| self.cards[port - 2].as_deref_mut())
                .flatten();
            if let Some(card) = card {
                let mut packet = bytes.clone();
                packet.resize(send.max(receive), 0);
                let replies = card.exchange(&packet);
                self.fifo_out.extend(&replies[..receive]);
                self.recv1 = RECV1_DEVICE;
                self.log_exchange(port, send, receive, bytes, out_before);
                continue;
            }
            let pad = (port < 2 && bytes.first() == Some(&0x01))
                .then(|| &mut self.pads[port])
                .filter(|p| p.connected);
            match pad {
                Some(pad) => {
                    pad.reset_transfer();
                    let replies: Vec<u8> = (0..send.max(receive))
                        .map(|k| pad.transfer(bytes.get(k).copied().unwrap_or(0)).0)
                        .collect();
                    self.fifo_out.extend(&replies[..receive]);
                    self.recv1 = RECV1_DEVICE;
                }
                None => {
                    self.fifo_out.extend(std::iter::repeat_n(0xFF, receive));
                    self.recv1 = RECV1_NO_DEVICE;
                }
            }
            self.log_exchange(port, send, receive, bytes, out_before);
        }
        self.fifo_in.clear();
        self.in_blocks.clear();
        self.intr |= 1;
    }

    /// Keeps the last commands, for diagnostics.
    fn log_exchange(&mut self, port: usize, send: usize, receive: usize, bytes: Vec<u8>, out_before: usize) {
        if self.log.len() == 24 {
            self.log.pop_front();
        }
        let got: Vec<u8> = self.fifo_out.iter().skip(out_before).copied().collect();
        if port >= 2 && self.card_log.len() < 200 {
            self.card_log.push((port, send, receive, bytes.clone(), got.clone()));
        }
        self.log.push_back((port, send, receive, bytes, got));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_controller_answers() {
        let mut s = Sio2::default();
        s.pads[0].buttons = texel_ps1::button::CROSS;
        // Port 0, 5 bytes each way: poll.
        s.write(0x00, (5 << 18) | (5 << 8));
        // The bytes come after the start, as by DMA.
        s.write(0x68, 1);
        assert!(!s.irq);
        s.push_in(&[0x01, 0x42, 0, 0, 0]);
        assert!(s.irq);
        assert_eq!(s.read(0x6C), RECV1_DEVICE);
        assert_eq!(
            (0..5).map(|_| s.pop_out()).collect::<Vec<_>>(),
            [0xFF, 0x41, 0x5A, 0xFF, 0xBF]
        );
    }

    #[test]
    fn transfers_complete_with_no_device() {
        let mut s = Sio2::default();
        s.pads[0].connected = false;
        // One command of 5 bytes each way.
        s.write(0x00, (5 << 18) | (5 << 8));
        for b in [0x01, 0x42, 0, 0, 0] {
            s.write(0x60, b);
        }
        s.write(0x68, 1);
        assert!(s.irq);
        assert_eq!(s.read(0x6C), RECV1_NO_DEVICE);
        assert_eq!(s.read(0x68) & 1, 0);
        assert_eq!((0..5).map(|_| s.read(0x64)).collect::<Vec<_>>(), [0xFF; 5]);
    }
}
