//! VU1's work, queued. What VIF1 asks of VU1 (data, microcode, programs to
//! run), its DIRECT data (path 2, kept in order with VU1's XGKICKs), and
//! the programs the EE starts, wait here in order; they run when something
//! needs what VU1 and the GIF and GS own (`Bus::sync_vu1`): the EE or a DMA
//! touching VU1's memory, the GIF's path 3, the GS's registers, the
//! vertical blank (the frame's picture is the GS's then), a save state.
//!
//! Those points depend on the EE's side only, so a run is the same each
//! time. What VU1 sends the GS reaches it then rather than when the program
//! starts: later than an instant VU1, as a real one, which runs beside the
//! EE, would.
//!
//! The queue is a stream of bytes (operations, with runs of quadwords or
//! words merged), so that it can be handed to another thread or worker as
//! it is: with a `Vu1Backend`, a `Vu1Engine` there runs VU1 while the EE
//! goes on, and what VU1 sent the GIF comes back as records (runs of
//! quadwords by path) the sync passes to the real GIF and GS, in order.
//! VU1's state comes back when the EE side reads VU1's memory or saves;
//! the EE side's writes to it go to the engine too. Without a backend, the
//! sync runs the operations itself (kept as a list, cheaper than encoding
//! them): the same ones, at the same points, so a machine with VU1 elsewhere
//! and one without go the same way (netplay, save states).

use crate::gif::Gif;
use crate::gs::Gs;
use crate::vif::{Direct, VuSink};
use crate::vu::Vu;

/// VU1's micro memory, in bytes.
const MICRO: u32 = 16 * 1024;

/// With a backend, a batch goes as soon as it is this long (bytes).
const BATCH: usize = 64 * 1024;

/// Operations.
mod op {
    /// Lanes of one vector: u32 qaddr, u8 mask (bit k: lane k), the lanes'
    /// u32s.
    pub const LANES: u8 = 1;
    /// Whole vectors at consecutive addresses: u32 qaddr, u32 n, n u128s.
    pub const VECTORS: u8 = 2;
    /// Micro memory words at consecutive addresses: u32 byte address, u32
    /// n, n u32s.
    pub const MICRO: u8 = 3;
    /// A program: u8 flags (1: pc, 2: itop, 4: top), then those given (u32
    /// pc, u16 itop, u16 top).
    pub const START: u8 = 4;
    /// DIRECT quadwords: u32 n, n u128s.
    pub const DIRECT: u8 = 5;
    pub const RESET: u8 = 6;
    /// The EE side's write to VU1's memory: u32 offset in its window
    /// (micro memory, then data), u8 n, n bytes.
    pub const POKE: u8 = 7;
    /// VU1's state and the GIF's, from the EE side (a state loaded): u32
    /// n, n bytes (postcard of both).
    pub const LOAD: u8 = 8;
    /// VU1's state wanted (a `STATE` record).
    pub const REPORT: u8 = 9;
}

/// Records, what comes back.
mod rec {
    /// Quadwords on a path: u8 path, u32 n, n u128s.
    pub const PATH: u8 = 1;
    /// An XGKICK's packet ends (the GIF's path 1 is reset).
    pub const KICK_END: u8 = 2;
    /// VU1's state: u32 n, n bytes (postcard).
    pub const STATE: u8 = 3;
}

/// An operation, as queued without a backend (encoding it would only cost
/// time).
#[derive(Clone, Copy, Debug)]
enum Op {
    Lanes {
        qaddr: u32,
        lanes: [Option<u32>; 4],
    },
    Micro {
        at: u32,
        word: u32,
    },
    Start {
        pc: Option<u32>,
        itop: Option<u16>,
        top: Option<u16>,
    },
    Direct(u128),
    Reset,
}

/// A byte stream of tagged entries, consecutive ones of a kind merged.
#[derive(Clone, Debug, Default)]
pub(crate) struct Stream {
    buf: Vec<u8>,
    /// The open run: its tag, key (a path), the address the next element
    /// must have to join it, and where its count is.
    run: Option<(u8, u32, u32, usize)>,
}

impl Stream {
    fn put(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// An entry that is not a run.
    fn tag(&mut self, tag: u8) {
        self.run = None;
        self.buf.push(tag);
    }

    /// One more element of a run: joins the open one when it can (`at`, the
    /// element's address, is the one expected), else starts one with the
    /// header bytes given (the count follows them). The element's bytes
    /// come next.
    fn element(&mut self, tag: u8, key: u32, at: u32, step: u32, header: &[u8]) {
        if let Some((t, k, next, count)) = &mut self.run
            && *t == tag
            && *k == key
            && *next == at
        {
            *next = at.wrapping_add(step);
            let c = &mut self.buf[*count..*count + 4];
            let n = u32::from_le_bytes((&*c).try_into().expect("4 bytes")) + 1;
            c.copy_from_slice(&n.to_le_bytes());
            return;
        }
        self.buf.push(tag);
        self.put(header);
        self.run = Some((tag, key, at.wrapping_add(step), self.buf.len()));
        self.put(&1u32.to_le_bytes());
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(crate) fn take(&mut self) -> Vec<u8> {
        self.run = None;
        std::mem::take(&mut self.buf)
    }

    /// A path's quadword (a record).
    pub(crate) fn path(&mut self, path: usize, q: u128) {
        self.element(rec::PATH, path as u32, 0, 0, &[path as u8]);
        self.put(&q.to_le_bytes());
    }

    pub(crate) fn kick_end(&mut self) {
        self.tag(rec::KICK_END);
    }

    fn sized(&mut self, tag: u8, bytes: &[u8]) {
        self.tag(tag);
        self.put(&(bytes.len() as u32).to_le_bytes());
        self.put(bytes);
    }
}

/// Reads a stream.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        head
    }
    fn u8(&mut self) -> u8 {
        self.bytes(1)[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.bytes(2).try_into().expect("2 bytes"))
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.bytes(4).try_into().expect("4 bytes"))
    }
    fn u128(&mut self) -> u128 {
        u128::from_le_bytes(self.bytes(16).try_into().expect("16 bytes"))
    }
}

/// Puts `new` (deserialised) in `vu`'s place, keeping what is not saved
/// (its compiled programs, which check micro memory again).
fn install(vu: &mut Vu, mut new: Vu) {
    new.jit = vu.jit.take();
    new.micro_gen = vu.micro_gen + 1;
    *vu = new;
}

/// Runs a stream of operations on `vu`, its XGKICKs and DIRECT data to
/// `gif` and `gs` (which, in an engine, record them).
fn execute(ops: &[u8], vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) {
    let mut r = Reader(ops);
    let mut d = Direct { vu, gif, gs };
    while !r.0.is_empty() {
        match r.u8() {
            op::LANES => {
                let qaddr = r.u32();
                let mask = r.u8();
                for k in 0..4 {
                    if mask & (1 << k) != 0 {
                        let v = r.u32();
                        d.vu.write_lane(qaddr, k, v);
                    }
                }
            }
            op::VECTORS => {
                let qaddr = r.u32();
                let qwords = d.vu.data.len() / 16;
                for i in 0..r.u32() {
                    let at = (qaddr.wrapping_add(i) as usize % qwords) * 16;
                    d.vu.data[at..at + 16].copy_from_slice(r.bytes(16));
                }
            }
            op::MICRO => {
                let at = r.u32() as usize;
                let n = r.u32() as usize;
                let len = d.vu.micro.len();
                for (i, w) in r.bytes(4 * n).as_chunks::<4>().0.iter().enumerate() {
                    let a = (at + 4 * i) % len;
                    d.vu.micro[a..a + 4].copy_from_slice(w);
                }
                d.vu.micro_gen += 1;
            }
            op::START => {
                let flags = r.u8();
                let pc = (flags & 1 != 0).then(|| r.u32());
                let itop = if flags & 2 != 0 { r.u16() } else { d.vu.itop };
                let top = (flags & 4 != 0).then(|| r.u16());
                d.start(pc, itop, top);
            }
            op::DIRECT => {
                for _ in 0..r.u32() {
                    let q = r.u128();
                    d.direct(q);
                }
            }
            op::RESET => d.vu.reset(),
            op::POKE => {
                let at = r.u32() as usize;
                let n = r.u8() as usize;
                let bytes = r.bytes(n);
                let micro = d.vu.micro.len();
                if at < micro {
                    d.vu.micro[at..at + n].copy_from_slice(bytes);
                    d.vu.micro_gen += 1;
                } else {
                    d.vu.data[at - micro..at - micro + n].copy_from_slice(bytes);
                }
            }
            op::LOAD => {
                let n = r.u32() as usize;
                let (vu, gif): (Vu, Gif) = postcard::from_bytes(r.bytes(n)).expect("a state from the EE side");
                install(d.vu, vu);
                let capture = d.gif.capture.take();
                *d.gif = gif;
                d.gif.capture = capture;
            }
            op::REPORT => {
                let state = postcard::to_stdvec(&*d.vu).expect("serialising to memory cannot fail");
                if let Some(c) = d.gif.capture.as_mut() {
                    c.sized(rec::STATE, &state);
                }
            }
            t => panic!("VU1 queue: unknown operation {t}"),
        }
    }
}

/// Passes what an engine recorded to the real GIF and GS, and VU1's state
/// to `vu`.
fn replay(records: &[u8], vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) {
    let mut r = Reader(records);
    while !r.0.is_empty() {
        match r.u8() {
            rec::PATH => {
                let path = r.u8() as usize;
                for _ in 0..r.u32() {
                    let q = r.u128();
                    gif.write(path, q, gs);
                }
            }
            rec::KICK_END => gif.end_kick(),
            rec::STATE => {
                let n = r.u32() as usize;
                install(vu, postcard::from_bytes(r.bytes(n)).expect("a state from the engine"));
            }
            t => panic!("VU1 queue: unknown record {t}"),
        }
    }
}

/// VU1 elsewhere: runs the batches it is given, in order, while the EE
/// goes on, and gives back what they recorded.
pub trait Vu1Backend {
    /// A batch of operations, to run.
    fn send(&mut self, batch: Vec<u8>);
    /// Waits for the batches sent to have run: what they recorded, in
    /// order.
    fn wait(&mut self) -> Vec<u8>;
}

/// VU1, run apart from the machine (in a thread or worker of its own): its
/// GIF records what it would send the GS.
pub struct Vu1Engine {
    pub vu: Vu,
    gif: Gif,
    /// Never written: the GIF records instead.
    gs: Gs,
}

impl Default for Vu1Engine {
    fn default() -> Self {
        let mut gif = Gif::default();
        gif.capture = Some(Stream::default());
        Self {
            vu: Vu::new(1),
            gif,
            gs: Gs::default(),
        }
    }
}

impl Vu1Engine {
    /// Runs a batch: what it recorded.
    pub fn process(&mut self, batch: &[u8]) -> Vec<u8> {
        execute(batch, &mut self.vu, &mut self.gif, &mut self.gs);
        self.gif.capture.as_mut().map(Stream::take).unwrap_or_default()
    }
}

/// VU1 in a thread of its own.
#[cfg(not(target_arch = "wasm32"))]
pub struct Thread {
    to: std::sync::mpsc::Sender<Vec<u8>>,
    from: std::sync::mpsc::Receiver<Vec<u8>>,
    sent: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl Thread {
    pub fn spawn() -> Self {
        let (to, batches) = std::sync::mpsc::channel::<Vec<u8>>();
        let (records, from) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("vu1".into())
            .spawn(move || {
                let mut engine = Vu1Engine::default();
                for batch in batches {
                    if records.send(engine.process(&batch)).is_err() {
                        break;
                    }
                }
            })
            .expect("starting VU1's thread");
        Self { to, from, sent: 0 }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Vu1Backend for Thread {
    fn send(&mut self, batch: Vec<u8>) {
        self.to.send(batch).expect("VU1's thread stopped");
        self.sent += 1;
    }

    fn wait(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..std::mem::take(&mut self.sent) {
            out.extend(self.from.recv().expect("VU1's thread stopped"));
        }
        out
    }
}

/// Why the queue ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The EE or a DMA reads or writes VU1's memory.
    VuMemory,
    /// The GIF's registers, or its path 3 (DMA, FIFO).
    Gif,
    /// The GS's privileged registers.
    Gs,
    VBlank,
    /// A save state (`Ps2::settle`).
    Save,
}

impl Why {
    pub const COUNT: usize = 5;
}

/// Counters, for diagnostics: what a VU1 beside the EE could take on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Operations queued, programs among them.
    pub ops: u64,
    pub starts: u64,
    /// Syncs that ran something, by reason (`Why` order).
    pub syncs: [u64; Why::COUNT],
    /// EE cycles between the oldest queued operation and its sync, summed:
    /// the time VU1's work could have run beside the EE.
    pub window: u64,
    /// Bytes queued, and batches sent to a backend.
    pub bytes: u64,
    pub batches: u64,
    /// Time the syncs took (natively; nanoseconds), and the part spent
    /// replaying records, for diagnostics.
    pub sync_ns: u64,
    pub replay_ns: u64,
}

#[derive(Default)]
pub struct Vu1Queue {
    /// Without a backend, the operations; with one, those not sent yet,
    /// encoded.
    list: Vec<Op>,
    ops: Stream,
    backend: Option<Box<dyn Vu1Backend>>,
    /// Batches sent whose records were not replayed yet.
    sent: bool,
    /// The engine ran something since VU1's state was last brought back
    /// (the bus's copy is behind).
    behind: bool,
    /// The bus's time (EE cycles), and when the oldest queued op came.
    pub now: u64,
    since: u64,
    pub stats: Stats,
}

impl Vu1Queue {
    /// Starts an operation.
    fn begin(&mut self) {
        if self.is_empty() {
            self.since = self.now;
        }
        self.stats.ops += 1;
        self.behind = true;
    }

    /// Ends one: with a backend, a long batch goes.
    fn end(&mut self) {
        if self.ops.buf.len() >= BATCH {
            self.send();
        }
    }

    fn send(&mut self) {
        if let Some(b) = self.backend.as_mut()
            && !self.ops.is_empty()
        {
            let batch = self.ops.take();
            self.stats.bytes += batch.len() as u64;
            self.stats.batches += 1;
            b.send(batch);
            self.sent = true;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty() && self.ops.is_empty() && !self.sent
    }

    /// Queues an operation without a backend: whether it was.
    fn listed(&mut self, op: Op) -> bool {
        if self.backend.is_some() {
            return false;
        }
        self.list.push(op);
        true
    }

    pub fn remote(&self) -> bool {
        self.backend.is_some()
    }

    /// Runs VU1 on `backend` from now on (None: here, at syncs), from the
    /// state of `vu` and `gif`. Nothing may be queued (after a sync).
    pub fn set_backend(&mut self, backend: Option<Box<dyn Vu1Backend>>, vu: &Vu, gif: &Gif) {
        assert!(self.is_empty(), "VU1's queue changes hands after a sync");
        self.backend = backend;
        self.load(vu, gif);
    }

    /// Takes the backend away, dropping what is queued (a state loaded:
    /// the new machine's queue gets it).
    pub fn take_backend(&mut self) -> Option<Box<dyn Vu1Backend>> {
        let mut b = self.backend.take()?;
        if self.sent {
            b.wait();
        }
        self.ops.take();
        self.list.clear();
        self.sent = false;
        Some(b)
    }

    /// The EE side's VU1 and GIF, now theirs (a state loaded), for the
    /// engine.
    pub fn load(&mut self, vu: &Vu, gif: &Gif) {
        if self.backend.is_none() {
            return;
        }
        let state = postcard::to_stdvec(&(vu, gif)).expect("serialising to memory cannot fail");
        if self.is_empty() {
            self.since = self.now;
        }
        self.ops.sized(op::LOAD, &state);
        self.behind = false;
        self.end();
    }

    /// The EE side wrote VU1's memory (`at`: in its window, micro memory
    /// then data), after a sync: the engine's copy too.
    pub fn poke(&mut self, at: u32, bytes: &[u8]) {
        if self.backend.is_none() {
            return;
        }
        if self.is_empty() {
            self.since = self.now;
        }
        self.ops.tag(op::POKE);
        self.ops.put(&at.to_le_bytes());
        self.ops.buf.push(bytes.len() as u8);
        self.ops.put(bytes);
        self.end();
    }

    /// The EE starts a program (CMSAR1).
    pub fn start_at(&mut self, pc: u32) {
        self.stats.starts += 1;
        self.begin();
        if self.listed(Op::Start {
            pc: Some(pc),
            itop: None,
            top: None,
        }) {
            return;
        }
        self.ops.tag(op::START);
        self.ops.buf.push(1);
        self.ops.put(&pc.to_le_bytes());
        self.end();
    }

    pub fn reset(&mut self) {
        self.begin();
        if self.listed(Op::Reset) {
            return;
        }
        self.ops.tag(op::RESET);
        self.end();
    }

    /// Runs what is queued, in order, on `vu` (its XGKICKs and the DIRECT
    /// data to `gif` and `gs`); with a backend, waits for it and replays
    /// what it recorded, and brings VU1's state back into `vu` when it is
    /// wanted (VU1's memory read or written, a save).
    pub fn run(&mut self, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs, why: Why) {
        let report = self.backend.is_some() && self.behind && matches!(why, Why::VuMemory | Why::Save);
        if self.is_empty() && !report {
            return;
        }
        self.stats.syncs[why as usize] += 1;
        self.stats.window += self.now - self.since;
        #[cfg(not(target_arch = "wasm32"))]
        let started = std::time::Instant::now();
        if self.backend.is_none() {
            let mut d = Direct { vu, gif, gs };
            for op in self.list.drain(..) {
                match op {
                    Op::Lanes { qaddr, lanes } => d.write_lanes(qaddr, lanes),
                    Op::Micro { at, word } => d.write_micro(at, word),
                    Op::Start { pc, itop, top } => {
                        let itop = itop.unwrap_or(d.vu.itop);
                        d.start(pc, itop, top);
                    }
                    Op::Direct(q) => d.direct(q),
                    Op::Reset => d.vu.reset(),
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.stats.sync_ns += started.elapsed().as_nanos() as u64;
            }
            return;
        }
        if report {
            self.ops.tag(op::REPORT);
            self.behind = false;
        }
        self.send();
        self.sent = false;
        let records = self.backend.as_mut().expect("a backend").wait();
        #[cfg(not(target_arch = "wasm32"))]
        let replaying = std::time::Instant::now();
        replay(&records, vu, gif, gs);
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.stats.replay_ns += replaying.elapsed().as_nanos() as u64;
            self.stats.sync_ns += started.elapsed().as_nanos() as u64;
        }
    }
}

impl VuSink for Vu1Queue {
    fn write_lanes(&mut self, qaddr: u32, lanes: [Option<u32>; 4]) {
        self.begin();
        if self.listed(Op::Lanes { qaddr, lanes }) {
            return;
        }
        if let [Some(a), Some(b), Some(c), Some(d)] = lanes {
            self.ops.element(op::VECTORS, 0, qaddr, 1, &qaddr.to_le_bytes());
            for v in [a, b, c, d] {
                self.ops.put(&v.to_le_bytes());
            }
        } else {
            self.ops.tag(op::LANES);
            self.ops.put(&qaddr.to_le_bytes());
            let mask = lanes
                .iter()
                .enumerate()
                .fold(0u8, |m, (k, v)| m | u8::from(v.is_some()) << k);
            self.ops.buf.push(mask);
            for v in lanes.into_iter().flatten() {
                self.ops.put(&v.to_le_bytes());
            }
        }
        self.end();
    }

    fn write_micro(&mut self, at: u32, word: u32) {
        self.begin();
        if self.listed(Op::Micro { at, word }) {
            return;
        }
        self.ops.element(op::MICRO, 0, at, 4, &at.to_le_bytes());
        self.ops.put(&word.to_le_bytes());
        self.end();
    }

    fn micro_len(&self) -> u32 {
        MICRO
    }

    fn start(&mut self, pc: Option<u32>, itop: u16, top: Option<u16>) {
        self.stats.starts += 1;
        self.begin();
        if self.listed(Op::Start {
            pc,
            itop: Some(itop),
            top,
        }) {
            return;
        }
        self.ops.tag(op::START);
        self.ops
            .buf
            .push(u8::from(pc.is_some()) | 2 | u8::from(top.is_some()) << 2);
        if let Some(pc) = pc {
            self.ops.put(&pc.to_le_bytes());
        }
        self.ops.put(&itop.to_le_bytes());
        if let Some(top) = top {
            self.ops.put(&top.to_le_bytes());
        }
        self.end();
    }

    fn direct(&mut self, q: u128) {
        self.begin();
        if self.listed(Op::Direct(q)) {
            return;
        }
        self.ops.element(op::DIRECT, 0, 0, 0, &[]);
        self.ops.put(&q.to_le_bytes());
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOP: u32 = 0x2FF;
    const LNOP: u32 = 0x8000_033C;
    const E: u32 = 1 << 30;

    /// Microcode, data, programs (one kicking a packet) and DIRECT data,
    /// and the EE side poking VU1's memory then reading it.
    fn feed(q: &mut Vu1Queue) {
        let iaddiu = |it: u32, imm: u32| (0x08 << 25) | (it << 16) | imm;
        let xgkick = |is: u32| (0x40 << 25) | (is << 11) | (0x1B << 6) | 0x3C;
        let prog = [(NOP, iaddiu(1, 0x10)), (NOP, xgkick(1)), (NOP | E, LNOP), (NOP, LNOP)];
        for (i, (upper, lower)) in prog.iter().enumerate() {
            q.write_micro(8 * i as u32, *lower);
            q.write_micro(8 * i as u32 + 4, *upper);
        }
        // At 0x10: a packet, 1 loop, EOP, packed, A+D (to FINISH).
        let tag = 1u128 | (1 << 15) | (1u128 << 60) | (0xEu128 << 64);
        let lanes = |v: u128| [0, 1, 2, 3].map(|k| Some((v >> (32 * k)) as u32));
        q.write_lanes(0x10, lanes(tag));
        q.write_lanes(0x11, lanes(7 | (0x61u128 << 64)));
        q.write_lanes(0x12, [Some(1), None, None, Some(2)]);
        q.start(Some(0), 3, Some(0x20));
        q.direct(tag);
        q.direct(5 | (0x61u128 << 64));
        q.start_at(0);
    }

    fn bytes<T: serde::Serialize>(v: &T) -> Vec<u8> {
        postcard::to_stdvec(v).unwrap()
    }

    /// The same operations, run here and by an engine in a thread, leave
    /// VU1, the GIF and the GS the same.
    #[test]
    fn engine_matches_inline() {
        let mut runs = Vec::new();
        for remote in [false, true] {
            let (mut vu, mut gif, mut gs) = (Vu::new(1), Gif::default(), Gs::default());
            let mut q = Vu1Queue::default();
            if remote {
                q.set_backend(Some(Box::new(Thread::spawn())), &vu, &gif);
            }
            feed(&mut q);
            q.run(&mut vu, &mut gif, &mut gs, Why::VBlank);
            // The EE side writes a word of VU1's data, then reads it all.
            q.run(&mut vu, &mut gif, &mut gs, Why::VuMemory);
            vu.data[0x40..0x44].copy_from_slice(&9u32.to_le_bytes());
            q.poke(0x4000 + 0x40, &9u32.to_le_bytes());
            feed(&mut q);
            q.run(&mut vu, &mut gif, &mut gs, Why::Save);
            assert!(q.is_empty());
            assert_eq!(gif.qwords[0], 8, "each program kicked its packet");
            assert_eq!(gif.qwords[1], 4);
            runs.push((vu.data.to_vec(), bytes(&vu), bytes(&gif), bytes(&gs)));
        }
        assert_eq!(runs[0], runs[1]);
    }
}
