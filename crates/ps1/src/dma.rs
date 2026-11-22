//! DMA controller registers. The transfers themselves run in the bus, which
//! owns the memory and the devices.

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Port {
    MdecIn = 0,
    MdecOut = 1,
    Gpu = 2,
    CdRom = 3,
    Spu = 4,
    Pio = 5,
    Otc = 6,
}

impl Port {
    fn from_index(i: u32) -> Self {
        match i {
            0 => Self::MdecIn,
            1 => Self::MdecOut,
            2 => Self::Gpu,
            3 => Self::CdRom,
            4 => Self::Spu,
            5 => Self::Pio,
            _ => Self::Otc,
        }
    }
}

#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Channel {
    pub base: u32,
    pub block: u32,
    pub control: u32,
}

impl Channel {
    pub fn is_from_ram(&self) -> bool {
        self.control & 1 != 0
    }

    pub fn decrement(&self) -> bool {
        self.control & 2 != 0
    }

    pub fn sync_mode(&self) -> u32 {
        (self.control >> 9) & 3
    }

    /// A transfer under way (or waiting to start): CHCR bit 24.
    pub fn busy(&self) -> bool {
        self.control & (1 << 24) != 0
    }

    /// Words to transfer for sync modes 0 (manual) and 1 (request).
    pub fn word_count(&self) -> u32 {
        let size = self.block & 0xffff;
        if self.sync_mode() == 0 {
            if size == 0 { 0x1_0000 } else { size }
        } else {
            // Blocks times block size; no blocks means nothing to transfer
            // (OpenBIOS relies on it, as PCSX-Redux behaves).
            let count = self.block >> 16;
            let size = if size == 0 { 0x1_0000 } else { size };
            size * count
        }
    }

    fn active(&self) -> bool {
        let enabled = self.control & (1 << 24) != 0;
        let trigger = self.sync_mode() != 0 || self.control & (1 << 28) != 0;
        enabled && trigger
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Dma {
    channels: [Channel; 7],
    control: u32,
    interrupt: u32,
}

impl Default for Dma {
    fn default() -> Self {
        Self {
            channels: [Channel::default(); 7],
            control: 0x0765_4321,
            interrupt: 0,
        }
    }
}

impl Dma {
    pub fn channel(&self, port: Port) -> Channel {
        self.channels[port as usize]
    }

    pub fn channel_mut(&mut self, port: Port) -> &mut Channel {
        &mut self.channels[port as usize]
    }

    /// Whether a transfer is waiting for its device (and is enabled in DPCR).
    pub fn pending(&self, port: Port) -> bool {
        let enabled = self.control & (8 << (port as u32 * 4)) != 0;
        enabled && self.channels[port as usize].active()
    }

    /// The master flag (DICR bit 31): the DMA interrupt is raised when it
    /// goes from 0 to 1.
    pub fn irq_flag(&self) -> bool {
        let force = self.interrupt & (1 << 15) != 0;
        let master = self.interrupt & (1 << 23) != 0;
        let enabled = (self.interrupt >> 16) & 0x7f;
        let flags = (self.interrupt >> 24) & 0x7f;
        force || (master && enabled & flags != 0)
    }

    pub fn read(&self, off: u32) -> u32 {
        let off = off & !3;
        match off {
            0x70 => self.control,
            0x74 => self.interrupt | (u32::from(self.irq_flag()) << 31),
            0x00..=0x6f => {
                let ch = &self.channels[(off >> 4) as usize];
                match off & 0xf {
                    0 => ch.base,
                    4 => ch.block,
                    8 => ch.control,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    /// Writes a register; returns the port of a transfer to start.
    pub fn write(&mut self, off: u32, v: u32) -> Option<Port> {
        match off {
            0x70 => self.control = v,
            0x74 => {
                // Bits 24-30 are acknowledged by writing 1.
                let ack = v & 0x7f00_0000;
                let flags = (self.interrupt & 0x7f00_0000) & !ack;
                self.interrupt = (v & 0x00ff_803f) | flags;
            }
            0x00..=0x6f => {
                let i = off >> 4;
                let ch = &mut self.channels[i as usize];
                match off & 0xf {
                    0 => ch.base = v & 0x00ff_ffff,
                    4 => ch.block = v,
                    8 => {
                        ch.control = if i == 6 { (v & 0x5100_0000) | 2 } else { v & 0x7177_0703 };
                        let port = Port::from_index(i);
                        if self.pending(port) {
                            return Some(port);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        None
    }

    /// Marks a transfer as done. Returns whether to raise the DMA interrupt.
    pub fn finish(&mut self, port: Port) -> bool {
        let ch = &mut self.channels[port as usize];
        ch.control &= !((1 << 24) | (1 << 28));
        let before = self.irq_flag();
        if self.interrupt & (1 << (16 + port as u32)) != 0 {
            self.interrupt |= 1 << (24 + port as u32);
        }
        !before && self.irq_flag()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Tarzan relies on: a flag left over from the BIOS is hidden while
    /// its channel is disabled, and raises the master flag again when the
    /// channel is enabled (the bus then raises the interrupt).
    #[test]
    fn master_flag_follows_enabled_flags() {
        let mut dma = Dma::default();
        // Channels 2 and 3 enabled, master enable on; a GPU transfer ends.
        dma.write(0x74, 0x008c_0000);
        assert!(dma.finish(Port::Gpu), "the master flag rises: interrupt");
        // Acknowledging another channel leaves the GPU's flag up.
        dma.write(0x74, 0x888c_0000);
        assert!(dma.irq_flag());
        assert!(!dma.finish(Port::CdRom), "already up: no new interrupt");
        // Writing 0 disables every channel, without acknowledging.
        dma.write(0x74, 0);
        assert!(!dma.irq_flag());
        assert_eq!(dma.read(0x74) & 0x7f00_0000, 0x0c00_0000);
        // Enabling channel 2 again brings its flag back.
        dma.write(0x74, 0x0084_0000);
        assert!(dma.irq_flag());
        // Acknowledged, it goes.
        dma.write(0x74, 0x0484_0000);
        assert!(!dma.irq_flag());
    }
}
