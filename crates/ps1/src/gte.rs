//! Geometry Transformation Engine (coprocessor 2): fixed-point vector and
//! matrix maths for 3D transformation, perspective and lighting.
//!
//! Follows the behaviour documented in psx-spx, including the 44-bit MAC
//! overflow checks, saturation flags and the UNR division.

const fn unr_table() -> [u8; 0x101] {
    let mut t = [0u8; 0x101];
    let mut i = 0;
    while i < 0x101 {
        let v = (0x40000 / (i as i32 + 0x100) + 1) / 2 - 0x101;
        t[i] = if v < 0 { 0 } else { v as u8 };
        i += 1;
    }
    t
}

const UNR_TABLE: [u8; 0x101] = unr_table();

// FLAG register bits.
const MAC1_POS: u32 = 1 << 30;
const MAC1_NEG: u32 = 1 << 27;
const IR1_SAT: u32 = 1 << 24;
const COLOR_R_SAT: u32 = 1 << 21;
const SZ_SAT: u32 = 1 << 18;
const DIV_OVERFLOW: u32 = 1 << 17;
const MAC0_POS: u32 = 1 << 16;
const MAC0_NEG: u32 = 1 << 15;
const SX_SAT: u32 = 1 << 14;
const SY_SAT: u32 = 1 << 13;
const IR0_SAT: u32 = 1 << 12;
const ERROR_MASK: u32 = 0x7f87_e000;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Gte {
    // Data registers.
    v: [[i16; 3]; 3],
    rgbc: [u8; 4],
    otz: u16,
    ir: [i16; 4],
    sxy: [[i16; 2]; 3],
    sz: [u16; 4],
    rgb_fifo: [[u8; 4]; 3],
    res1: u32,
    mac: [i32; 4],
    lzcs: u32,
    // Control registers.
    rt: [[i16; 3]; 3],
    tr: [i32; 3],
    llm: [[i16; 3]; 3],
    bk: [i32; 3],
    lcm: [[i16; 3]; 3],
    fc: [i32; 3],
    ofx: i32,
    ofy: i32,
    h: u16,
    dqa: i16,
    dqb: i32,
    zsf3: i16,
    zsf4: i16,
    flag: u32,
    // Current command fields.
    sf: u32,
    lm: bool,
    /// Enhancements (not the console's own behaviour; not in save states):
    /// keep projections' exact positions (`projected`), and squeeze X by 3/4
    /// for a 16:9 picture.
    #[serde(skip)]
    pub precise: bool,
    #[serde(skip)]
    pub widescreen: bool,
    /// The last command's projections, exactly: the screen position it was
    /// rounded to (packed as SXY), and its x, y and depth.
    #[serde(skip)]
    pub projected: Vec<(u32, [f32; 3])>,
}

fn lo(v: u32) -> i16 {
    v as i16
}

fn hi(v: u32) -> i16 {
    (v >> 16) as i16
}

fn pack(a: i16, b: i16) -> u32 {
    u32::from(a as u16) | (u32::from(b as u16) << 16)
}

impl Gte {
    pub fn read_data(&self, reg: u32) -> u32 {
        match reg {
            0 | 2 | 4 => pack(self.v[reg as usize / 2][0], self.v[reg as usize / 2][1]),
            1 | 3 | 5 => self.v[reg as usize / 2][2] as i32 as u32,
            6 => u32::from_le_bytes(self.rgbc),
            7 => u32::from(self.otz),
            8..=11 => self.ir[reg as usize - 8] as i32 as u32,
            12..=14 => pack(self.sxy[reg as usize - 12][0], self.sxy[reg as usize - 12][1]),
            15 => pack(self.sxy[2][0], self.sxy[2][1]),
            16..=19 => u32::from(self.sz[reg as usize - 16]),
            20..=22 => u32::from_le_bytes(self.rgb_fifo[reg as usize - 20]),
            23 => self.res1,
            24..=27 => self.mac[reg as usize - 24] as u32,
            28 | 29 => {
                let c = |i: usize| (i32::from(self.ir[i]) >> 7).clamp(0, 0x1f) as u32;
                c(1) | (c(2) << 5) | (c(3) << 10)
            }
            30 => self.lzcs,
            31 => {
                let v = self.lzcs;
                if v & 0x8000_0000 != 0 {
                    (!v).leading_zeros()
                } else {
                    v.leading_zeros()
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn write_data(&mut self, reg: u32, val: u32) {
        match reg {
            0 | 2 | 4 => {
                let v = &mut self.v[reg as usize / 2];
                v[0] = lo(val);
                v[1] = hi(val);
            }
            1 | 3 | 5 => self.v[reg as usize / 2][2] = lo(val),
            6 => self.rgbc = val.to_le_bytes(),
            7 => self.otz = val as u16,
            8..=11 => self.ir[reg as usize - 8] = lo(val),
            12..=14 => self.sxy[reg as usize - 12] = [lo(val), hi(val)],
            15 => {
                self.sxy[0] = self.sxy[1];
                self.sxy[1] = self.sxy[2];
                self.sxy[2] = [lo(val), hi(val)];
            }
            16..=19 => self.sz[reg as usize - 16] = val as u16,
            20..=22 => self.rgb_fifo[reg as usize - 20] = val.to_le_bytes(),
            23 => self.res1 = val,
            24..=27 => self.mac[reg as usize - 24] = val as i32,
            28 => {
                self.ir[1] = ((val & 0x1f) << 7) as i16;
                self.ir[2] = (((val >> 5) & 0x1f) << 7) as i16;
                self.ir[3] = (((val >> 10) & 0x1f) << 7) as i16;
            }
            30 => self.lzcs = val,
            _ => {} // ORGB and LZCR are read-only.
        }
    }

    pub fn read_control(&self, reg: u32) -> u32 {
        let m = |m: &[[i16; 3]; 3], r: u32| match r {
            0 => pack(m[0][0], m[0][1]),
            1 => pack(m[0][2], m[1][0]),
            2 => pack(m[1][1], m[1][2]),
            3 => pack(m[2][0], m[2][1]),
            _ => m[2][2] as i32 as u32,
        };
        match reg {
            0..=4 => m(&self.rt, reg),
            5..=7 => self.tr[reg as usize - 5] as u32,
            8..=12 => m(&self.llm, reg - 8),
            13..=15 => self.bk[reg as usize - 13] as u32,
            16..=20 => m(&self.lcm, reg - 16),
            21..=23 => self.fc[reg as usize - 21] as u32,
            24 => self.ofx as u32,
            25 => self.ofy as u32,
            // H reads back sign-extended (a hardware quirk).
            26 => self.h as i16 as i32 as u32,
            27 => self.dqa as i32 as u32,
            28 => self.dqb as u32,
            29 => self.zsf3 as i32 as u32,
            30 => self.zsf4 as i32 as u32,
            31 => self.flag,
            _ => unreachable!(),
        }
    }

    pub fn write_control(&mut self, reg: u32, val: u32) {
        fn m(m: &mut [[i16; 3]; 3], r: u32, val: u32) {
            match r {
                0 => (m[0][0], m[0][1]) = (lo(val), hi(val)),
                1 => (m[0][2], m[1][0]) = (lo(val), hi(val)),
                2 => (m[1][1], m[1][2]) = (lo(val), hi(val)),
                3 => (m[2][0], m[2][1]) = (lo(val), hi(val)),
                _ => m[2][2] = lo(val),
            }
        }
        match reg {
            0..=4 => m(&mut self.rt, reg, val),
            5..=7 => self.tr[reg as usize - 5] = val as i32,
            8..=12 => m(&mut self.llm, reg - 8, val),
            13..=15 => self.bk[reg as usize - 13] = val as i32,
            16..=20 => m(&mut self.lcm, reg - 16, val),
            21..=23 => self.fc[reg as usize - 21] = val as i32,
            24 => self.ofx = val as i32,
            25 => self.ofy = val as i32,
            26 => self.h = val as u16,
            27 => self.dqa = lo(val),
            28 => self.dqb = val as i32,
            29 => self.zsf3 = lo(val),
            30 => self.zsf4 = lo(val),
            31 => {
                self.flag = val & 0x7fff_f000;
                self.update_flag_summary();
            }
            _ => unreachable!(),
        }
    }

    fn update_flag_summary(&mut self) {
        if self.flag & ERROR_MASK != 0 {
            self.flag |= 0x8000_0000;
        } else {
            self.flag &= 0x7fff_ffff;
        }
    }

    /// Executes a GTE command (the low 25 bits of a COP2 instruction).
    pub fn command(&mut self, cmd: u32) {
        self.sf = if cmd & (1 << 19) != 0 { 12 } else { 0 };
        self.projected.clear();
        self.lm = cmd & (1 << 10) != 0;
        self.flag = 0;
        match cmd & 0x3f {
            0x01 => self.rtps(0, true),
            0x06 => self.nclip(),
            0x0c => self.op(),
            0x10 => self.dpcs(false),
            0x11 => self.intpl(),
            0x12 => self.mvmva(cmd),
            0x13 => self.ncd(0),
            0x14 => self.cdp(),
            0x16 => (0..3).for_each(|i| self.ncd(i)),
            0x1b => self.ncc(0),
            0x1c => self.cc(),
            0x1e => self.nc(0),
            0x20 => (0..3).for_each(|i| self.nc(i)),
            0x28 => self.sqr(),
            0x29 => self.dcpl(),
            0x2a => (0..3).for_each(|_| self.dpcs(true)),
            0x2d => self.avsz3(),
            0x2e => self.avsz4(),
            0x30 => (0..3).for_each(|i| self.rtps(i, i == 2)),
            0x3d => self.gpf(),
            0x3e => self.gpl(),
            0x3f => (0..3).for_each(|i| self.ncc(i)),
            _ => {}
        }
        self.update_flag_summary();
    }

    // --- Saturation and overflow helpers -------------------------------

    /// Checks a MAC1-3 intermediate against 44 bits, and wraps it.
    fn check_mac(&mut self, n: usize, v: i64) -> i64 {
        if v > 0x7ff_ffff_ffff {
            self.flag |= MAC1_POS >> (n - 1);
        } else if v < -0x800_0000_0000 {
            self.flag |= MAC1_NEG >> (n - 1);
        }
        (v << 20) >> 20
    }

    fn set_mac(&mut self, n: usize, v: i64) -> i64 {
        let v = self.check_mac(n, v) >> self.sf;
        self.mac[n] = v as i32;
        v
    }

    fn set_mac0(&mut self, v: i64) -> i64 {
        if v > 0x7fff_ffff {
            self.flag |= MAC0_POS;
        } else if v < -0x8000_0000 {
            self.flag |= MAC0_NEG;
        }
        self.mac[0] = v as i32;
        v
    }

    fn set_ir(&mut self, n: usize, v: i64, lm: bool) {
        let min = if lm { 0 } else { -0x8000 };
        let c = v.clamp(min, 0x7fff);
        if c != v {
            self.flag |= IR1_SAT >> (n - 1);
        }
        self.ir[n] = c as i16;
    }

    fn set_ir0(&mut self, v: i64) {
        let c = v.clamp(0, 0x1000);
        if c != v {
            self.flag |= IR0_SAT;
        }
        self.ir[0] = c as i16;
    }

    /// MACn = value >> sf, IRn = saturate(MACn).
    fn mac_ir(&mut self, n: usize, v: i64, lm: bool) {
        let v = self.set_mac(n, v);
        self.set_ir(n, v, lm);
    }

    fn push_color(&mut self) {
        let mut c = [0u8; 4];
        for i in 0..3 {
            let v = self.mac[i + 1] >> 4;
            let s = v.clamp(0, 0xff);
            if s != v {
                self.flag |= COLOR_R_SAT >> i;
            }
            c[i] = s as u8;
        }
        c[3] = self.rgbc[3];
        self.rgb_fifo[0] = self.rgb_fifo[1];
        self.rgb_fifo[1] = self.rgb_fifo[2];
        self.rgb_fifo[2] = c;
    }

    fn push_sz(&mut self, v: i64) {
        let c = v.clamp(0, 0xffff);
        if c != v {
            self.flag |= SZ_SAT;
        }
        self.sz = [self.sz[1], self.sz[2], self.sz[3], c as u16];
    }

    fn push_sxy(&mut self, x: i64, y: i64) {
        let cx = x.clamp(-0x400, 0x3ff);
        if cx != x {
            self.flag |= SX_SAT;
        }
        let cy = y.clamp(-0x400, 0x3ff);
        if cy != y {
            self.flag |= SY_SAT;
        }
        self.sxy = [self.sxy[1], self.sxy[2], [cx as i16, cy as i16]];
    }

    /// matrix * vector + (translation << 12), with per-term overflow checks.
    fn mat_vec(&mut self, m: &[[i16; 3]; 3], v: [i64; 3], t: [i64; 3]) -> [i64; 3] {
        let mut out = [0; 3];
        for (row, o) in out.iter_mut().enumerate() {
            let n = row + 1;
            let mut acc = self.check_mac(n, t[row] << 12);
            for col in 0..3 {
                acc = self.check_mac(n, acc + i64::from(m[row][col]) * v[col]);
            }
            *o = acc;
        }
        out
    }

    fn ir_vec(&self) -> [i64; 3] {
        [self.ir[1].into(), self.ir[2].into(), self.ir[3].into()]
    }

    fn v_vec(&self, i: usize) -> [i64; 3] {
        self.v[i].map(i64::from)
    }

    // --- Commands -------------------------------------------------------

    fn rtps(&mut self, i: usize, last: bool) {
        let rt = self.rt;
        let t = self.tr.map(i64::from);
        let r = self.mat_vec(&rt, self.v_vec(i), t);
        self.mac_ir(1, r[0], self.lm);
        self.mac_ir(2, r[1], self.lm);
        let mac3 = self.set_mac(3, r[2]);
        // IR3's flag is checked against MAC3 >> 12 whatever sf is.
        let ir3 = mac3.clamp(if self.lm { 0 } else { -0x8000 }, 0x7fff);
        let unshifted = r[2] >> 12;
        if unshifted != unshifted.clamp(-0x8000, 0x7fff) {
            self.flag |= IR1_SAT >> 2;
        }
        self.ir[3] = ir3 as i16;
        self.push_sz(r[2] >> 12);
        let flags = self.flag;
        let div = self.unr_divide();
        let ir1 = i64::from(self.ir[1]);
        // Widescreen: X squeezed around the centre (the picture is then
        // shown 4/3 wider).
        let dx = if self.widescreen { div * ir1 * 3 / 4 } else { div * ir1 };
        let x = self.set_mac0(dx + i64::from(self.ofx));
        let y = self.set_mac0(div * i64::from(self.ir[2]) + i64::from(self.ofy));
        self.push_sxy(x >> 16, y >> 16);
        // The exact position, from the unsaturated, unrounded values, when
        // nothing saturated or overflowed (else the rounded one is all).
        let clean = (self.flag & !flags) & (SX_SAT | SY_SAT | DIV_OVERFLOW) == 0;
        if self.precise && clean {
            let scale = if self.sf == 12 { 4096.0 } else { 1.0 };
            let (px, py, pz) = (r[0] as f64 / scale, r[1] as f64 / scale, r[2] as f64 / 4096.0);
            if pz >= 1.0 && px.abs() <= 32767.0 && py.abs() <= 32767.0 {
                let h = f64::from(self.h) / pz;
                let squeeze = if self.widescreen { 0.75 } else { 1.0 };
                let sx = f64::from(self.ofx) / 65536.0 + px * h * squeeze;
                let sy = f64::from(self.ofy) / 65536.0 + py * h;
                let [cx, cy] = self.sxy[2];
                self.projected.push((pack(cx, cy), [sx as f32, sy as f32, pz as f32]));
            }
        }
        if last {
            let d = self.set_mac0(div * i64::from(self.dqa) + i64::from(self.dqb));
            self.set_ir0(d >> 12);
        }
    }

    fn unr_divide(&mut self) -> i64 {
        let h = u32::from(self.h);
        let sz3 = u32::from(self.sz[3]);
        if h >= sz3 * 2 {
            self.flag |= DIV_OVERFLOW;
            return 0x1ffff;
        }
        let z = (sz3 as u16).leading_zeros();
        let n = u64::from(h << z);
        let d = sz3 << z;
        let u = u32::from(UNR_TABLE[((d - 0x7fc0) >> 7) as usize]) + 0x101;
        let d = (0x200_0080 - d * u) >> 8;
        let d = (0x80 + d * u) >> 8;
        (((n * u64::from(d)) + 0x8000) >> 16).min(0x1ffff) as i64
    }

    fn nclip(&mut self) {
        let [s0, s1, s2] = self.sxy.map(|s| [i64::from(s[0]), i64::from(s[1])]);
        let v = s0[0] * s1[1] + s1[0] * s2[1] + s2[0] * s0[1] - s0[0] * s2[1] - s1[0] * s0[1] - s2[0] * s1[1];
        self.set_mac0(v);
    }

    fn op(&mut self) {
        let [ir1, ir2, ir3] = self.ir_vec();
        let (d1, d2, d3) = (
            i64::from(self.rt[0][0]),
            i64::from(self.rt[1][1]),
            i64::from(self.rt[2][2]),
        );
        let lm = self.lm;
        self.mac_ir(1, ir3 * d2 - ir2 * d3, lm);
        self.mac_ir(2, ir1 * d3 - ir3 * d1, lm);
        self.mac_ir(3, ir2 * d1 - ir1 * d2, lm);
    }

    /// MAC += (FC << 12 - MAC) * IR0, through IR, then shifted into
    /// MAC/IR and pushed to the colour FIFO. Takes the unshifted MACs.
    fn interpolate_and_push(&mut self, mac: [i64; 3]) {
        let fc = self.fc.map(i64::from);
        let sf = self.sf;
        let mut ir = [0i64; 3];
        for i in 0..3 {
            let v = self.check_mac(i + 1, (fc[i] << 12) - mac[i]) >> sf;
            self.set_ir(i + 1, v, false);
            ir[i] = self.ir[i + 1].into();
        }
        let ir0 = i64::from(self.ir[0]);
        let lm = self.lm;
        for i in 0..3 {
            let v = self.check_mac(i + 1, ir[i] * ir0 + mac[i]);
            self.mac_ir(i + 1, v, lm);
        }
        self.push_color();
    }

    fn push_shifted(&mut self, mac: [i64; 3]) {
        let lm = self.lm;
        for (i, v) in mac.into_iter().enumerate() {
            self.mac_ir(i + 1, v, lm);
        }
        self.push_color();
    }

    fn rgb_times_ir(&mut self) -> [i64; 3] {
        let ir = self.ir_vec();
        [0, 1, 2].map(|i| self.check_mac(i + 1, (i64::from(self.rgbc[i]) * ir[i]) << 4))
    }

    fn dpcs(&mut self, fifo: bool) {
        let c = if fifo { self.rgb_fifo[0] } else { self.rgbc };
        self.interpolate_and_push([0, 1, 2].map(|i| i64::from(c[i]) << 16));
    }

    fn intpl(&mut self) {
        let ir = self.ir_vec();
        self.interpolate_and_push(ir.map(|v| v << 12));
    }

    fn dcpl(&mut self) {
        let m = self.rgb_times_ir();
        self.interpolate_and_push(m);
    }

    fn sqr(&mut self) {
        let ir = self.ir_vec();
        for (i, v) in ir.into_iter().enumerate() {
            self.mac_ir(i + 1, v * v, self.lm);
        }
    }

    fn avsz3(&mut self) {
        let sum = i64::from(self.sz[1]) + i64::from(self.sz[2]) + i64::from(self.sz[3]);
        let v = self.set_mac0(i64::from(self.zsf3) * sum);
        self.set_otz(v >> 12);
    }

    fn avsz4(&mut self) {
        let sum: i64 = self.sz.iter().map(|&z| i64::from(z)).sum();
        let v = self.set_mac0(i64::from(self.zsf4) * sum);
        self.set_otz(v >> 12);
    }

    fn set_otz(&mut self, v: i64) {
        let c = v.clamp(0, 0xffff);
        if c != v {
            self.flag |= SZ_SAT;
        }
        self.otz = c as u16;
    }

    fn gpf(&mut self) {
        let ir = self.ir_vec();
        let ir0 = i64::from(self.ir[0]);
        self.push_shifted(ir.map(|v| v * ir0));
    }

    fn gpl(&mut self) {
        let ir = self.ir_vec();
        let ir0 = i64::from(self.ir[0]);
        let sf = self.sf;
        let mac = [0, 1, 2].map(|i| {
            let base = i64::from(self.mac[i + 1]) << sf;
            self.check_mac(i + 1, base + ir[i] * ir0)
        });
        self.push_shifted(mac);
    }

    fn mvmva(&mut self, cmd: u32) {
        let mx = match (cmd >> 17) & 3 {
            0 => self.rt,
            1 => self.llm,
            2 => self.lcm,
            // Reserved: a garbage matrix made of RGBC, IR0 and RT13/RT22.
            _ => {
                let r = i16::from(self.rgbc[0]) << 4;
                [[-r, r, self.ir[0]], [self.rt[0][2]; 3], [self.rt[1][1]; 3]]
            }
        };
        let v = match (cmd >> 15) & 3 {
            0..=2 => self.v_vec(((cmd >> 15) & 3) as usize),
            _ => self.ir_vec(),
        };
        let lm = self.lm;
        match (cmd >> 13) & 3 {
            2 => {
                // FC as translation is buggy: the first column's result is
                // only used for flags, the result omits it.
                let fc = self.fc.map(i64::from);
                for row in 0..3 {
                    let n = row + 1;
                    let first = self.check_mac(n, (fc[row] << 12) + i64::from(mx[row][0]) * v[0]);
                    self.set_ir(n, first >> self.sf, false);
                    let mut acc = self.check_mac(n, i64::from(mx[row][1]) * v[1]);
                    acc = self.check_mac(n, acc + i64::from(mx[row][2]) * v[2]);
                    self.mac_ir(n, acc, lm);
                }
            }
            cv => {
                let t = match cv {
                    0 => self.tr.map(i64::from),
                    1 => self.bk.map(i64::from),
                    _ => [0; 3],
                };
                let r = self.mat_vec(&mx, v, t);
                for (i, val) in r.into_iter().enumerate() {
                    self.mac_ir(i + 1, val, lm);
                }
            }
        }
    }

    /// Light matrix and light colour stages of the normal-colour commands:
    /// IR = LCM * (LLM * V) + BK.
    fn light(&mut self, i: usize) {
        let llm = self.llm;
        let r = self.mat_vec(&llm, self.v_vec(i), [0; 3]);
        let lm = self.lm;
        for (n, val) in r.into_iter().enumerate() {
            self.mac_ir(n + 1, val, lm);
        }
        self.color_matrix();
    }

    fn color_matrix(&mut self) {
        let lcm = self.lcm;
        let bk = self.bk.map(i64::from);
        let r = self.mat_vec(&lcm, self.ir_vec(), bk);
        let lm = self.lm;
        for (n, val) in r.into_iter().enumerate() {
            self.mac_ir(n + 1, val, lm);
        }
    }

    fn nc(&mut self, i: usize) {
        self.light(i);
        self.push_color();
    }

    fn ncc(&mut self, i: usize) {
        self.light(i);
        let m = self.rgb_times_ir();
        self.push_shifted(m);
    }

    fn ncd(&mut self, i: usize) {
        self.light(i);
        let m = self.rgb_times_ir();
        self.interpolate_and_push(m);
    }

    fn cc(&mut self) {
        self.color_matrix();
        let m = self.rgb_times_ir();
        self.push_shifted(m);
    }

    fn cdp(&mut self) {
        self.color_matrix();
        let m = self.rgb_times_ir();
        self.interpolate_and_push(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unr_table_matches_known_values() {
        assert_eq!(UNR_TABLE[0], 0xff);
        assert_eq!(UNR_TABLE[1], 0xfd);
        assert_eq!(UNR_TABLE[0x100], 0);
    }

    #[test]
    fn rtps_projects_a_point() {
        let mut g = Gte::default();
        // Identity rotation (1.0 = 0x1000), no translation.
        g.write_control(0, 0x1000);
        g.write_control(2, 0x1000);
        g.write_control(4, 0x1000);
        g.write_control(24, 160 << 16); // OFX
        g.write_control(25, 120 << 16); // OFY
        g.write_control(26, 200); // H
        g.write_data(0, pack(100, -50));
        g.write_data(1, 400);
        g.command((1 << 19) | 0x01);
        // x = 160 + 100 * 200 / 400 = 210, y = 120 - 25 = 95.
        let sxy = g.read_data(14);
        assert_eq!((lo(sxy), hi(sxy)), (210, 95));
        assert_eq!(g.read_data(19), 400);
        assert_eq!(g.read_control(31) & 0x8000_0000, 0);
    }

    #[test]
    fn nclip_and_avsz3() {
        let mut g = Gte::default();
        g.write_data(12, pack(0, 0));
        g.write_data(13, pack(10, 0));
        g.write_data(14, pack(0, 10));
        g.command(0x06);
        assert_eq!(g.read_data(24) as i32, 100);
        g.write_control(29, 0x555); // ~ 1/3 in 4.12
        g.write_data(17, 300);
        g.write_data(18, 300);
        g.write_data(19, 300);
        g.command(0x2d);
        assert_eq!(g.read_data(7), (0x555 * 900) >> 12);
    }

    #[test]
    fn irgb_and_lzcr() {
        let mut g = Gte::default();
        g.write_data(28, 0x7fff);
        assert_eq!(g.read_data(9), 0xf80);
        assert_eq!(g.read_data(29), 0x7fff);
        g.write_data(30, 0x0000_ffff);
        assert_eq!(g.read_data(31), 16);
        g.write_data(30, 0xffff_0000);
        assert_eq!(g.read_data(31), 16);
    }
}
