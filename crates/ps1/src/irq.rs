//! Interrupt controller (I_STAT, I_MASK).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Sio: serial port 1 is not emulated.
pub enum Interrupt {
    VBlank = 0,
    Gpu = 1,
    CdRom = 2,
    Dma = 3,
    Timer0 = 4,
    Timer1 = 5,
    Timer2 = 6,
    Controller = 7,
    Sio = 8,
    Spu = 9,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Irq {
    status: u32,
    mask: u32,
}

impl Irq {
    pub fn raise(&mut self, i: Interrupt) {
        self.status |= 1 << i as u32;
    }

    /// Whether the interrupt line to the CPU (COP0 cause bit 10) is up.
    pub fn active(&self) -> bool {
        self.status & self.mask != 0
    }

    pub fn read(&self, off: u32) -> u32 {
        if off < 4 { self.status } else { self.mask }
    }

    pub fn write(&mut self, off: u32, v: u32) {
        if off < 4 {
            // Writing 0 acknowledges.
            self.status &= v & 0x7ff;
        } else {
            self.mask = v & 0x7ff;
        }
    }
}
