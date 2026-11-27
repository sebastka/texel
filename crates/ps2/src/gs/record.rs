//! What reaches the GS, recorded in order for another renderer (the GPU's,
//! in `texel-ps2-gpu`): draws with the registers they depend on, transfers
//! into its memory, copies within it, and CLUT loads.

use super::{Gs, Vertex, field, reg};

/// Who renders: the software GS, a recorder, or both (to compare them);
/// or nobody (`Off`: measuring the rest of the console).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Rendering {
    #[default]
    Software,
    Record,
    Both,
    Off,
}

impl Rendering {
    pub fn software(self) -> bool {
        matches!(self, Rendering::Software | Rendering::Both)
    }

    pub fn records(self) -> bool {
        matches!(self, Rendering::Record | Rendering::Both)
    }
}

/// The registers a draw depends on, for its context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawRegs {
    /// PRIM, or PRMODE (with PRIM's type) when PRMODECONT says so.
    pub prim: u64,
    pub frame: u64,
    pub zbuf: u64,
    pub test: u64,
    pub alpha: u64,
    pub scissor: u64,
    pub xyoffset: u64,
    pub tex0: u64,
    pub tex1: u64,
    /// Mipmap levels 1 to 3, and 4 to 6.
    pub miptbp1: u64,
    pub miptbp2: u64,
    pub clamp: u64,
    pub fba: u64,
    pub texa: u64,
    pub fogcol: u64,
    pub colclamp: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// A primitive: its kind (0 point, 1 line, 2 line strip, 3 triangle,
    /// 4 strip, 5 fan, 6 sprite) and vertices.
    Draw {
        kind: u32,
        regs: DrawRegs,
        vertices: Vec<Vertex>,
    },
    /// Pixels written by a host to local transfer, in raster order over a
    /// rectangle, from pixel `first` of it.
    Pixels {
        bp: u32,
        bw: u32,
        psm: u32,
        x: u32,
        y: u32,
        w: u32,
        first: u32,
        data: Vec<u32>,
    },
    /// A local to local transfer.
    Copy {
        sbp: u32,
        sbw: u32,
        spsm: u32,
        sx: u32,
        sy: u32,
        dbp: u32,
        dbw: u32,
        dpsm: u32,
        dx: u32,
        dy: u32,
        w: u32,
        h: u32,
    },
    /// A CLUT load, as TEX0 asks (its CLD said to load).
    Clut { tex0: u64 },
}

impl Gs {
    /// The registers the next draw depends on.
    pub(super) fn draw_regs(&self) -> DrawRegs {
        let r = &self.regs;
        let prim = r[reg::PRIM as usize];
        let prim = if r[reg::PRMODECONT as usize] & 1 != 0 {
            prim
        } else {
            (r[reg::PRMODE as usize] & !7) | (prim & 7)
        };
        let c = field(prim, 9, 1) as usize;
        DrawRegs {
            prim,
            frame: r[reg::FRAME_1 as usize + c],
            zbuf: r[reg::ZBUF_1 as usize + c],
            test: r[reg::TEST_1 as usize + c],
            alpha: r[reg::ALPHA_1 as usize + c],
            scissor: r[reg::SCISSOR_1 as usize + c],
            xyoffset: r[reg::XYOFFSET_1 as usize + c],
            tex0: r[reg::TEX0_1 as usize + c],
            tex1: r[reg::TEX1_1 as usize + c],
            miptbp1: r[reg::MIPTBP1_1 as usize + c],
            miptbp2: r[reg::MIPTBP2_1 as usize + c],
            clamp: r[reg::CLAMP_1 as usize + c],
            fba: r[reg::FBA_1 as usize + c],
            texa: r[reg::TEXA as usize],
            fogcol: r[reg::FOGCOL as usize],
            colclamp: r[reg::COLCLAMP as usize],
        }
    }

    /// Records a transferred pixel: appended to the last command when it
    /// continues it.
    pub(super) fn record_pixel(&mut self, target: (u32, u32, u32, u32, u32, u32), index: u32, value: u32) {
        let (bp, bw, psm, x, y, w) = target;
        if let Some(Command::Pixels {
            bp: b,
            bw: bwl,
            psm: p,
            x: xl,
            y: yl,
            w: wl,
            first,
            data,
        }) = self.commands.last_mut()
            && (*b, *bwl, *p, *xl, *yl, *wl) == (bp, bw, psm, x, y, w)
            && *first + data.len() as u32 == index
        {
            data.push(value);
            return;
        }
        self.commands.push(Command::Pixels {
            bp,
            bw,
            psm,
            x,
            y,
            w,
            first: index,
            data: vec![value],
        });
    }

    /// The commands recorded since the last call.
    /// Commands recorded so far (diagnostics).
    pub fn recorded(&self) -> usize {
        self.commands.len()
    }

    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }
}
