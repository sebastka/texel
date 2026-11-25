//! The memory map and the peripherals hanging off it.

use crate::cdrom::CdRom;
use crate::dma::{Dma, Port};
use crate::gpu::Gpu;
use crate::irq::{Interrupt, Irq};
use crate::mdec::Mdec;
use crate::sio::Sio;
use crate::spu::Spu;
use crate::timers::Timers;

pub const RAM_SIZE: usize = 2 * 1024 * 1024;
pub const BIOS_SIZE: usize = 512 * 1024;

/// Strips the segment (KUSEG, KSEG0, KSEG1) from a virtual address.
fn physical(addr: u32) -> u32 {
    const MASKS: [u32; 8] = [
        0xffff_ffff,
        0xffff_ffff,
        0xffff_ffff,
        0xffff_ffff,
        0x7fff_ffff,
        0x1fff_ffff,
        0xffff_ffff,
        0xffff_ffff,
    ];
    addr & MASKS[(addr >> 29) as usize]
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Bus {
    pub ram: Box<[u8]>,
    /// Not in save states: the system keeps its BIOS.
    #[serde(skip)]
    pub bios: Box<[u8]>,
    #[serde(with = "serde_big_array::BigArray")]
    pub(crate) scratchpad: [u8; 1024],
    mem_ctrl: [u32; 9],
    ram_size: u32,
    cache_ctrl: u32,
    /// Cycles the CPU ran (or DMA took) that the peripherals have not caught
    /// up with yet; see [`Bus::sync`].
    pub pending: u32,
    /// When `pending` reaches this, the peripherals catch up: set by each
    /// sync (see [`Bus::sync`]).
    #[serde(skip, default = "slice")]
    pub deadline: u32,
    /// Diagnostics: the deadline every sync sets.
    #[serde(skip, default = "slice")]
    pub slice: u32,
    /// Writes to RAM, by 256-byte page (wrapping): compiled blocks compare
    /// their code with RAM only when its pages' counts changed (see
    /// `jit`). Not saved: restoring a state drops the compiled blocks.
    #[serde(skip, default = "ram_pages")]
    pub ram_writes: Box<[u32]>,
    /// Cycles the peripherals have caught up with, since the start
    /// (diagnostics: with `pending`, the time so far).
    #[serde(skip)]
    pub elapsed: u64,
    /// A GPU command list transferred, its transfer ending when the GPU
    /// has drawn all but its last commands (see `tick`): the drawing they
    /// leave. Saved apart (`crate::Timing`); older states end it at once
    /// (`settle`).
    #[serde(skip)]
    pub(crate) gpu_list: Option<u64>,
    pub irq: Irq,
    pub dma: Dma,
    pub timers: Timers,
    pub gpu: Gpu,
    pub cdrom: CdRom,
    pub spu: Spu,
    pub mdec: Mdec,
    pub sio: Sio,
}

impl Bus {
    pub fn new(bios: Box<[u8]>) -> Self {
        Self {
            ram: vec![0; RAM_SIZE].into_boxed_slice(),
            bios,
            scratchpad: [0; 1024],
            mem_ctrl: [0; 9],
            ram_size: 0,
            cache_ctrl: 0,
            pending: 0,
            deadline: SLICE,
            ram_writes: ram_pages(),
            slice: SLICE,
            elapsed: 0,
            gpu_list: None,
            irq: Irq::default(),
            dma: Dma::default(),
            timers: Timers::default(),
            gpu: Gpu::new(),
            cdrom: CdRom::new(),
            spu: Spu::new(),
            mdec: Mdec::new(),
            sio: Sio::new(),
        }
    }

    /// Whether the instruction cache is enabled (cache control bit 11).
    pub fn icache_enabled(&self) -> bool {
        self.cache_ctrl & 0x800 != 0
    }

    /// Brings the peripherals up to the CPU's time. Done before every I/O
    /// access, so that what the CPU reads (timers, status registers) is
    /// exact, and regularly by the main loop.
    pub fn sync(&mut self) {
        while self.pending > 0 {
            let cycles = std::mem::take(&mut self.pending);
            self.tick(cycles);
        }
        self.deadline = self.next_event();
    }

    /// The CPU cycles from the last sync until a peripheral may raise an
    /// interrupt or end a frame, at most `slice`: when the next sync is
    /// due. Worked out again after every I/O access, which may start
    /// something.
    fn next_event(&mut self) -> u32 {
        let mut next = self.slice.min(self.gpu.cycles_to_vblank());
        let mut at = |n: Option<u32>| {
            if let Some(n) = n {
                next = next.min(n);
            }
        };
        if self.gpu.irq_raised {
            at(Some(1));
        }
        if let Some(last) = self.gpu_list {
            let left = self.gpu.drawing().saturating_sub(last);
            at(Some(left.clamp(1, u64::from(u32::MAX)) as u32));
        }
        at(self.timers.next_event(&self.gpu));
        at(self.cdrom.next_event());
        at(self.spu.next_event());
        at(self.sio.next_event());
        // Transfers waiting for their device's data are retried at syncs.
        if self.dma.pending(Port::MdecOut) || self.dma.pending(Port::CdRom) {
            at(Some(RETRY));
        }
        next.max(1)
    }

    /// Instruction fetch: only RAM and BIOS hold code.
    pub fn fetch(&mut self, addr: u32) -> u32 {
        let p = physical(addr);
        if p < 0x0080_0000 {
            let o = (p as usize) & (RAM_SIZE - 1);
            read_u32(&self.ram, o)
        } else if (0x1fc0_0000..0x1fc8_0000).contains(&p) {
            read_u32(&self.bios, (p - 0x1fc0_0000) as usize)
        } else {
            self.load32(addr)
        }
    }

    pub fn load32(&mut self, addr: u32) -> u32 {
        let p = physical(addr);
        match p {
            0x0000_0000..=0x007f_ffff => read_u32(&self.ram, (p as usize) & (RAM_SIZE - 1)),
            0x1f80_0000..=0x1f80_03ff => read_u32(&self.scratchpad, (p & 0x3ff) as usize),
            0x1fc0_0000..=0x1fc7_ffff => read_u32(&self.bios, (p - 0x1fc0_0000) as usize),
            0x1f80_1000..=0x1f80_1fff => self.io_read(p, 4),
            0xfffe_0130 => self.cache_ctrl,
            _ => 0xffff_ffff,
        }
    }

    pub fn load16(&mut self, addr: u32) -> u16 {
        let p = physical(addr);
        match p {
            0x0000_0000..=0x007f_ffff => read_u16(&self.ram, (p as usize) & (RAM_SIZE - 1)),
            0x1f80_0000..=0x1f80_03ff => read_u16(&self.scratchpad, (p & 0x3ff) as usize),
            0x1fc0_0000..=0x1fc7_ffff => read_u16(&self.bios, (p - 0x1fc0_0000) as usize),
            0x1f80_1000..=0x1f80_1fff => self.io_read(p, 2) as u16,
            _ => 0xffff,
        }
    }

    pub fn load8(&mut self, addr: u32) -> u8 {
        let p = physical(addr);
        match p {
            0x0000_0000..=0x007f_ffff => self.ram[(p as usize) & (RAM_SIZE - 1)],
            0x1f80_0000..=0x1f80_03ff => self.scratchpad[(p & 0x3ff) as usize],
            0x1fc0_0000..=0x1fc7_ffff => self.bios[(p - 0x1fc0_0000) as usize],
            0x1f80_1000..=0x1f80_1fff => self.io_read(p, 1) as u8,
            _ => 0xff,
        }
    }

    /// Counts a write at `o` in RAM (see `ram_writes`).
    #[inline]
    fn wrote_ram(&mut self, o: usize) {
        let n = &mut self.ram_writes[o >> RAM_PAGE_BITS];
        *n = n.wrapping_add(1);
    }

    pub fn store32(&mut self, addr: u32, v: u32) {
        let p = physical(addr);
        match p {
            0x0000_0000..=0x007f_ffff => {
                let o = (p as usize) & (RAM_SIZE - 1);
                self.wrote_ram(o);
                write_u32(&mut self.ram, o, v);
            }
            0x1f80_0000..=0x1f80_03ff => write_u32(&mut self.scratchpad, (p & 0x3ff) as usize, v),
            0x1f80_1000..=0x1f80_1fff => self.io_write(p, v, 4),
            0xfffe_0130 => self.cache_ctrl = v,
            _ => {}
        }
    }

    pub fn store16(&mut self, addr: u32, v: u16) {
        let p = physical(addr);
        match p {
            0x0000_0000..=0x007f_ffff => {
                let o = (p as usize) & (RAM_SIZE - 1);
                self.wrote_ram(o);
                write_u16(&mut self.ram, o, v);
            }
            0x1f80_0000..=0x1f80_03ff => write_u16(&mut self.scratchpad, (p & 0x3ff) as usize, v),
            0x1f80_1000..=0x1f80_1fff => self.io_write(p, u32::from(v), 2),
            _ => {}
        }
    }

    pub fn store8(&mut self, addr: u32, v: u8) {
        let p = physical(addr);
        match p {
            0x0000_0000..=0x007f_ffff => {
                let o = (p as usize) & (RAM_SIZE - 1);
                self.wrote_ram(o);
                self.ram[o] = v;
            }
            0x1f80_0000..=0x1f80_03ff => self.scratchpad[(p & 0x3ff) as usize] = v,
            0x1f80_1000..=0x1f80_1fff => self.io_write(p, u32::from(v), 1),
            _ => {}
        }
    }

    fn io_read(&mut self, p: u32, size: u32) -> u32 {
        self.sync();
        let v = self.io_read_at(p, size);
        self.deadline = self.next_event();
        v
    }

    fn io_read_at(&mut self, p: u32, size: u32) -> u32 {
        let off = p - 0x1f80_1000;
        match off {
            0x000..=0x023 => self.mem_ctrl[(off / 4) as usize] >> ((off & 3) * 8),
            0x040..=0x05f => self.sio.read(off - 0x40, size),
            0x060..=0x063 => self.ram_size,
            0x070..=0x077 => self.irq.read(off - 0x70) >> ((off & 3) * 8),
            0x080..=0x0ff => self.dma.read(off - 0x80) >> ((off & 3) * 8),
            0x100..=0x12f => self.timers.read(off - 0x100),
            0x800..=0x803 => {
                // 16/32-bit reads of the CD-ROM ports repeat the byte.
                let b = u32::from(self.cdrom.read(off - 0x800));
                match size {
                    1 => b,
                    2 => b | (b << 8),
                    _ => b * 0x0101_0101,
                }
            }
            0x810 => self.gpu.read_data(),
            0x814 => self.gpu.status(),
            0x820 => self.mdec.read_data(),
            0x824 => self.mdec.status(),
            0xc00..=0xfff => {
                let lo = u32::from(self.spu.read16(off & !1));
                if size == 4 {
                    lo | (u32::from(self.spu.read16((off & !1) + 2)) << 16)
                } else {
                    lo >> ((off & 1) * 8)
                }
            }
            _ => 0,
        }
    }

    fn io_write(&mut self, p: u32, v: u32, size: u32) {
        self.sync();
        self.io_write_at(p, v, size);
        self.deadline = self.next_event();
    }

    fn io_write_at(&mut self, p: u32, v: u32, size: u32) {
        let off = p - 0x1f80_1000;
        match off {
            0x000..=0x023 => self.mem_ctrl[(off / 4) as usize] = v,
            0x040..=0x05f => self.sio.write(off - 0x40, v, size),
            0x060..=0x063 => self.ram_size = v,
            0x070..=0x077 => self.irq.write((off - 0x70) & !3, v),
            0x080..=0x0ff => {
                let v = v << ((off & 3) * 8);
                let before = self.dma.irq_flag();
                let start = self.dma.write((off - 0x80) & !3, v);
                // The master flag also rises when DICR enables a channel
                // whose flag is still set: that raises the interrupt too, so
                // the game sees (and acknowledges) the flag. Otherwise the
                // flag stays up and no DMA interrupt is raised again: the
                // BIOS leaves one behind when its CD handler takes a GPU
                // transfer's interrupt during the boot animation, and the
                // movies of games (Tarzan) then wait for their DMA forever.
                if !before && self.dma.irq_flag() {
                    self.irq.raise(Interrupt::Dma);
                }
                if let Some(port) = start {
                    self.run_dma(port);
                }
            }
            0x100..=0x12f => self.timers.write(off - 0x100, v),
            0x800..=0x803 => self.cdrom.write(off - 0x800, v as u8),
            0x810 => {
                // A command waits for the drawing under way (the GPU's
                // command FIFO, approximately); then the GPU draws it while
                // the CPU goes on.
                self.pending += self.gpu.wait_drawing();
                self.gpu.gp0(v);
            }
            0x814 => self.gpu.gp1(v),
            0x820 => self.mdec.write_command(v),
            0x824 => self.mdec.write_control(v),
            0xc00..=0xfff => {
                if size == 4 {
                    self.spu.write16(off, v as u16);
                    self.spu.write16(off + 2, (v >> 16) as u16);
                } else {
                    self.spu.write16(off & !1, v as u16);
                }
            }
            _ => {}
        }
    }

    /// Runs a DMA transfer to completion, if the device is ready for it.
    pub fn run_dma(&mut self, port: Port) {
        // A command list already sent, its drawing under way.
        if port == Port::Gpu && self.gpu_list.is_some() {
            return;
        }
        let ready = match port {
            Port::MdecOut => self.mdec.output_ready(),
            Port::CdRom => self.cdrom.data_ready(),
            _ => true,
        };
        if !ready {
            // Retried by `tick` once the device has data.
            return;
        }
        let ch = self.dma.channel(port);
        let to_ram = !ch.is_from_ram();
        let step: u32 = if ch.decrement() { 0u32.wrapping_sub(4) } else { 4 };
        let mut addr = ch.base & 0x1f_fffc;
        let mut dma_words = ch.word_count();
        let mut last_drawing = 0;
        if ch.sync_mode() == 2 {
            dma_words = 0;
            if port == Port::Gpu {
                (dma_words, last_drawing) = self.dma_linked_list(ch.base);
                // The address then holds the end marker: started again,
                // the channel sends nothing.
                self.dma.channel_mut(port).base = 0x00ff_ffff;
            }
        } else if port == Port::Otc {
            let words = ch.word_count();
            for n in 0..words {
                let v = if n == words - 1 {
                    0x00ff_ffff
                } else {
                    addr.wrapping_sub(4) & 0x1f_ffff
                };
                self.wrote_ram(addr as usize);
                write_u32(&mut self.ram, addr as usize, v);
                addr = addr.wrapping_sub(4) & 0x1f_fffc;
            }
        } else {
            let words = ch.word_count();
            for _ in 0..words {
                let a = (addr & 0x1f_fffc) as usize;
                if to_ram {
                    let v = match port {
                        Port::MdecOut => self.mdec.read_data(),
                        Port::Gpu => self.gpu.read_data(),
                        Port::CdRom => self.cdrom.read_data_word(),
                        Port::Spu => self.spu.dma_read(),
                        _ => 0xffff_ffff,
                    };
                    self.wrote_ram(a);
                    write_u32(&mut self.ram, a, v);
                } else {
                    let v = read_u32(&self.ram, a);
                    match port {
                        Port::MdecIn => self.mdec.write_command(v),
                        Port::Gpu => self.gpu.gp0(v),
                        Port::Spu => self.spu.dma_write(v),
                        _ => {}
                    }
                }
                addr = addr.wrapping_add(step);
            }
            if ch.sync_mode() == 1 {
                self.dma.channel_mut(port).base = addr & 0x1f_fffc;
            }
        }
        // The CPU is halted while the DMA controller owns the bus. A
        // command list feeds the GPU as it asks for more, the CPU running in
        // between: the transfer ends when its last commands are in the GPU's
        // FIFO, the GPU having drawn the others (see `tick`). Other
        // transfers to the GPU (VRAM uploads) wait for the drawing.
        if port == Port::Gpu && ch.sync_mode() == 2 {
            self.pending += dma_words + 4;
            if self.gpu.drawing() > last_drawing {
                self.gpu_list = Some(last_drawing);
                return;
            }
        } else {
            self.pending += dma_words + 4;
            if port == Port::Gpu {
                self.pending += self.gpu.wait_drawing();
            }
        }
        if self.dma.finish(port) {
            self.irq.raise(Interrupt::Dma);
        }
    }

    /// Sends a GPU command list; returns the words transferred, and the
    /// drawing its last commands (its last node's) add.
    ///
    /// A list that loops back to a node it sent ends there. Tekken 3 sends
    /// some, at the end of its fights: sent round and round (up to a bound),
    /// they held its picture and sound for half a second each.
    fn dma_linked_list(&mut self, mut addr: u32) -> (u32, u64) {
        let mut total = 0;
        let mut last = 0;
        if addr & 0x0080_0000 != 0 {
            // At its end already.
            return (0, 0);
        }
        // A second walker, at half speed: the first meets it again in a
        // loop (Floyd's cycle detection: no memory of the nodes needed).
        let mut slow = addr;
        // Bounded too, as the walk is.
        for step in 0..0x10_0000u32 {
            let header = read_u32(&self.ram, (addr & 0x1f_fffc) as usize);
            let words = header >> 24;
            total += words + 1;
            let before = self.gpu.drawing();
            for n in 1..=words {
                let a = (addr.wrapping_add(n * 4) & 0x1f_fffc) as usize;
                self.gpu.gp0(read_u32(&self.ram, a));
            }
            if words > 0 {
                last = self.gpu.drawing().saturating_sub(before);
            }
            if header & 0x0080_0000 != 0 {
                break;
            }
            addr = header & 0x1f_fffc;
            if step % 2 == 1 {
                slow = read_u32(&self.ram, (slow & 0x1f_fffc) as usize) & 0x1f_fffc;
            }
            if addr == slow & 0x1f_fffc {
                break;
            }
        }
        (total, last)
    }

    /// Advances every peripheral by `cycles` CPU cycles.
    /// After a state of an older format is restored: a command list's
    /// transfer that was waiting for its drawing ends (their drawing time
    /// was not saved).
    pub fn settle(&mut self) {
        let gpu = self.dma.channel(Port::Gpu);
        if gpu.busy() && gpu.sync_mode() == 2 && self.dma.finish(Port::Gpu) {
            self.irq.raise(Interrupt::Dma);
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        self.elapsed += u64::from(cycles);
        let gpu = self.gpu.tick(cycles);
        // A command list's transfer ends when the GPU is drawing its last
        // commands.
        if self.gpu_list.is_some_and(|last| self.gpu.drawing() <= last) {
            self.gpu_list = None;
            if self.dma.finish(Port::Gpu) {
                self.irq.raise(Interrupt::Dma);
            }
            // A transfer started meanwhile goes now.
            if self.dma.pending(Port::Gpu) {
                self.run_dma(Port::Gpu);
            }
        }
        if gpu.vblank_start {
            self.irq.raise(Interrupt::VBlank);
        }
        if std::mem::take(&mut self.gpu.irq_raised) {
            self.irq.raise(Interrupt::Gpu);
        }
        self.timers.tick(cycles, &gpu, &mut self.irq);
        self.cdrom.tick(cycles, &mut self.irq, &mut self.spu);
        if self.spu.tick(cycles) {
            self.irq.raise(Interrupt::Spu);
        }
        self.sio.tick(cycles, &mut self.irq);
        for port in [Port::MdecOut, Port::CdRom] {
            if self.dma.pending(port) {
                self.run_dma(port);
            }
        }
    }
}

/// The cycles the CPU runs before the peripherals catch up, at most (when
/// none has anything to do sooner: see `Bus::next_event`).
pub const SLICE: u32 = 1 << 15;

/// How soon a transfer waiting for its device's data is tried again.
const RETRY: u32 = 64;

fn slice() -> u32 {
    SLICE
}

/// RAM's pages for `Bus::ram_writes`: 256 bytes.
pub const RAM_PAGE_BITS: u32 = 8;

fn ram_pages() -> Box<[u32]> {
    vec![0; RAM_SIZE >> RAM_PAGE_BITS].into_boxed_slice()
}

pub fn read_u32(mem: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([mem[o], mem[o + 1], mem[o + 2], mem[o + 3]])
}

pub fn read_u16(mem: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([mem[o], mem[o + 1]])
}

pub fn write_u32(mem: &mut [u8], o: usize, v: u32) {
    mem[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn write_u16(mem: &mut [u8], o: usize, v: u16) {
    mem[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command list at `at`: one node per command.
    fn list(bus: &mut Bus, at: u32, commands: &[&[u32]]) {
        let mut addr = at;
        for (k, words) in commands.iter().enumerate() {
            let next = addr + 4 * (words.len() as u32 + 1);
            let link = if k + 1 == commands.len() { 0x00ff_ffff } else { next };
            write_u32(&mut bus.ram, addr as usize, ((words.len() as u32) << 24) | link);
            for (n, &w) in words.iter().enumerate() {
                write_u32(&mut bus.ram, (addr + 4 * (n as u32 + 1)) as usize, w);
            }
            addr = next;
        }
    }

    fn send(bus: &mut Bus, at: u32) {
        bus.store32(0x1f80_10f0, 0x0000_0800);
        bus.store32(0x1f80_10a0, at);
        bus.store32(0x1f80_10a4, 0);
        bus.store32(0x1f80_10a8, 0x0100_0401);
    }

    fn busy(bus: &mut Bus) -> bool {
        bus.load32(0x1f80_10a8) & 0x0100_0000 != 0
    }

    // Filling all of VRAM: a long drawing.
    const FILL: &[u32] = &[0x0200_0000, 0, 0x01ff_03ff];

    #[test]
    fn a_command_list_ends_with_its_last_command_in_the_fifo() {
        // libgpu then sends its next request at once (waiting for the GPU
        // to be ready), rather than queuing it: Crash Team Racing queues
        // requests that point into its stack, sent garbage later.
        let mut bus = Bus::new(vec![0; 512 * 1024].into_boxed_slice());
        list(&mut bus, 0x1000, &[FILL]);
        send(&mut bus, 0x1000);
        assert!(!busy(&mut bus), "the transfer has ended");
        assert!(bus.gpu.drawing() > 0, "the GPU is still drawing");
        // Its address then holds the end marker: nothing to send again.
        assert_eq!(bus.load32(0x1f80_10a0), 0x00ff_ffff);
        let drawing = bus.gpu.drawing();
        bus.store32(0x1f80_10a8, 0x0100_0401);
        assert_eq!(bus.gpu.drawing(), drawing, "nothing sent");
    }

    #[test]
    fn a_command_list_ends_where_it_loops() {
        let mut once = Bus::new(vec![0; 512 * 1024].into_boxed_slice());
        list(&mut once, 0x1000, &[FILL, &[0xe100_0000]]);
        send(&mut once, 0x1000);
        let fill = once.gpu.drawing();
        // The same list, its second node leading back to the first.
        let mut bus = Bus::new(vec![0; 512 * 1024].into_boxed_slice());
        list(&mut bus, 0x1000, &[FILL, &[0xe100_0000]]);
        write_u32(&mut bus.ram, 0x1000 + 4 * 4, (1 << 24) | 0x1000);
        send(&mut bus, 0x1000);
        let drawing = bus.gpu.drawing();
        assert!(
            drawing >= fill && drawing <= 3 * fill,
            "drawn {drawing}, one fill {fill}"
        );
    }

    #[test]
    fn a_command_list_waits_for_all_but_its_last_command() {
        let mut bus = Bus::new(vec![0; 512 * 1024].into_boxed_slice());
        list(&mut bus, 0x1000, &[FILL, &[0xe100_0000]]);
        send(&mut bus, 0x1000);
        assert!(busy(&mut bus), "the fill is drawing");
        while bus.gpu.drawing() > 0 {
            bus.pending += 1000;
            bus.sync();
        }
        assert!(!busy(&mut bus), "the transfer has ended");
    }
}
