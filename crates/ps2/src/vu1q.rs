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
//! With a `Vu1Backend`, the queue is a stream of bytes (VIF1's words, the
//! EE's starts and resets), handed to another thread or worker as it is: a
//! `Vu1Engine` there runs a VIF1 and VU1 of its own while the EE goes on, and what VU1 sent the GIF comes back as records (runs of
//! quadwords by path) the sync passes to the real GIF and GS, in order.
//! VU1's state comes back when the EE side reads VU1's memory or saves;
//! the EE side's writes to it go to the engine too. Without a backend, VU1
//! runs at once on the bus's VU1 (nothing else sees it before a sync), its
//! GIF output recorded the same way and replayed at the same points: a
//! machine with VU1 elsewhere and one without go the same way (netplay,
//! save states).

use crate::gif::Gif;
use crate::gs::Gs;
use crate::vif::{Direct, Vif, VuSink};
use crate::vu::Vu;

/// VU1's micro memory, in bytes.
const MICRO: u32 = 16 * 1024;

/// With a backend, a batch goes as soon as it is this long (bytes).
const BATCH: usize = 64 * 1024;

/// Operations.
mod op {
    /// A program the EE starts: u8 flags (1: pc, 2: itop, 4: top), then those given (u32
    /// pc, u16 itop, u16 top).
    pub const START: u8 = 4;
    pub const RESET: u8 = 6;
    /// The EE side's write to VU1's memory: u32 offset in its window
    /// (micro memory, then data), u8 n, n bytes.
    pub const POKE: u8 = 7;
    /// VU1's, the GIF's and VIF1's states, from the EE side (a state
    /// loaded): u32 n, n bytes (postcard of the three).
    pub const LOAD: u8 = 8;
    /// VU1's state wanted (a `STATE` record).
    pub const REPORT: u8 = 9;
    /// VIF1's words, for the engine's VIF1: u32 n, n u32s.
    pub const VIF: u8 = 10;
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
        self.elements(tag, key, at, step, header, 1);
    }

    /// `n` elements (`step` apart), as `element`.
    fn elements(&mut self, tag: u8, key: u32, at: u32, step: u32, header: &[u8], n: u32) {
        let after = at.wrapping_add(step.wrapping_mul(n));
        if let Some((t, k, next, count)) = &mut self.run
            && *t == tag
            && *k == key
            && *next == at
        {
            *next = after;
            let c = &mut self.buf[*count..*count + 4];
            let total = u32::from_le_bytes((&*c).try_into().expect("4 bytes")) + n;
            c.copy_from_slice(&total.to_le_bytes());
            return;
        }
        self.buf.push(tag);
        self.put(header);
        self.run = Some((tag, key, after, self.buf.len()));
        self.put(&n.to_le_bytes());
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Takes `buf` back (emptied) for its room.
    fn reuse(&mut self, mut buf: Vec<u8>) {
        if self.buf.is_empty() {
            buf.clear();
            self.buf = buf;
        }
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

    /// A path's quadwords, as bytes (a record).
    pub(crate) fn path_bytes(&mut self, path: usize, bytes: &[u8]) {
        self.elements(rec::PATH, path as u32, 0, 0, &[path as u8], (bytes.len() / 16) as u32);
        self.put(bytes);
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
fn execute(ops: &[u8], vu: &mut Vu, gif: &mut Gif, gs: &mut Gs, vif: &mut Vif, words: &mut Vec<u32>) {
    let mut r = Reader(ops);
    let mut d = Direct { vu, gif, gs };
    while !r.0.is_empty() {
        match r.u8() {
            op::START => {
                let flags = r.u8();
                let pc = (flags & 1 != 0).then(|| r.u32());
                let itop = if flags & 2 != 0 { r.u16() } else { d.vu.itop };
                let top = (flags & 4 != 0).then(|| r.u16());
                d.start(pc, itop, top);
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
                let (vu, gif, v): (Vu, Gif, Vif) = postcard::from_bytes(r.bytes(n)).expect("a state from the EE side");
                *vif = v;
                install(d.vu, vu);
                let capture = d.gif.capture.take();
                *d.gif = gif;
                d.gif.capture = capture;
            }
            op::VIF => {
                let n = r.u32() as usize;
                words.clear();
                words.extend(r.bytes(4 * n).as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w)));
                vif.write(words, &mut d);
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
                let n = r.u32() as usize;
                for q in r.bytes(16 * n).as_chunks::<16>().0 {
                    gif.write(path, u128::from_le_bytes(*q), gs);
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
    /// VIF1, fed the EE side's VIF1's words (its UNPACKs, MPGs, MSCALs and
    /// DIRECTs done here).
    vif: Vif,
    words: Vec<u32>,
}

impl Default for Vu1Engine {
    fn default() -> Self {
        let mut gif = Gif::default();
        gif.capture = Some(Stream::default());
        Self {
            vu: Vu::new(1),
            gif,
            gs: Gs::default(),
            vif: Vif::new(1),
            words: Vec::new(),
        }
    }
}

impl Vu1Engine {
    /// Runs a batch: what it recorded.
    pub fn process(&mut self, batch: &[u8]) -> Vec<u8> {
        execute(
            batch,
            &mut self.vu,
            &mut self.gif,
            &mut self.gs,
            &mut self.vif,
            &mut self.words,
        );
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

pub struct Vu1Queue {
    /// With a backend: the operations not sent yet, encoded.
    ops: Stream,
    backend: Option<Box<dyn Vu1Backend>>,
    /// Batches sent whose records were not replayed yet.
    sent: bool,
    /// The engine ran something since VU1's state was last brought back
    /// (the bus's copy is behind).
    behind: bool,
    /// Without a backend, VU1 runs at once, on the bus's VU1; its GIF
    /// output waits for the sync all the same, recorded by this GIF (as an
    /// engine's), which never writes this GS.
    gif: Gif,
    gs: Box<Gs>,
    /// The bus's time (EE cycles), and when the oldest queued op came.
    pub now: u64,
    since: u64,
    pub stats: Stats,
}

impl Default for Vu1Queue {
    fn default() -> Self {
        let mut gif = Gif::default();
        gif.capture = Some(Stream::default());
        Self {
            ops: Stream::default(),
            backend: None,
            sent: false,
            behind: false,
            gif,
            gs: Box::default(),
            now: 0,
            since: 0,
            stats: Stats::default(),
        }
    }
}

/// What VIF1 and the EE ask of VU1, through the queue (`Vu1Queue::feed`).
pub struct Feed<'a> {
    q: &'a mut Vu1Queue,
    vu: &'a mut Vu,
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

    fn recorded(&self) -> bool {
        self.gif.capture.as_ref().is_some_and(|c| !c.is_empty())
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && !self.sent && !self.recorded()
    }

    pub fn remote(&self) -> bool {
        self.backend.is_some()
    }

    /// VU1 (the bus's), fed through the queue.
    pub fn feed<'a>(&'a mut self, vu: &'a mut Vu) -> Feed<'a> {
        Feed { q: self, vu }
    }

    /// VIF1's words: to `vif`, acting on VU1 (`vu`) here or, with VU1
    /// elsewhere, to the engine's VIF1 too (`vif` then keeps only its
    /// registers and counts).
    pub fn vif1(&mut self, vif: &mut Vif, vu: &mut Vu, words: &[u32]) {
        if self.backend.is_some() {
            self.begin();
            self.ops.elements(op::VIF, 0, 0, 0, &[], words.len() as u32);
            for w in words {
                self.ops.put(&w.to_le_bytes());
            }
            self.end();
        }
        vif.write(words, &mut self.feed(vu));
    }

    /// Runs VU1 on `backend` from now on (None: here), from the state of
    /// `vu` and `gif`. Nothing may be queued (after a sync).
    pub fn set_backend(&mut self, backend: Option<Box<dyn Vu1Backend>>, vu: &Vu, gif: &Gif, vif: &Vif) {
        assert!(self.is_empty(), "VU1's queue changes hands after a sync");
        self.backend = backend;
        self.load(vu, gif, vif);
    }

    /// Takes the backend away, dropping what is queued (a state loaded:
    /// the new machine's queue gets it).
    pub fn take_backend(&mut self) -> Option<Box<dyn Vu1Backend>> {
        if let Some(c) = self.gif.capture.as_mut() {
            c.take();
        }
        let mut b = self.backend.take()?;
        if self.sent {
            b.wait();
        }
        self.ops.take();
        self.sent = false;
        Some(b)
    }

    /// The EE side's VU1 and GIF, now theirs (a state loaded): for the
    /// engine, or for the GIF recording here.
    pub fn load(&mut self, vu: &Vu, gif: &Gif, vif: &Vif) {
        if self.backend.is_none() {
            let capture = self.gif.capture.take();
            self.gif = gif.clone();
            self.gif.capture = capture;
            return;
        }
        let state = postcard::to_stdvec(&(vu, gif, vif)).expect("serialising to memory cannot fail");
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

    /// Without a backend: VU1, acting at once, its GIF recording.
    fn here<'a>(&'a mut self, vu: &'a mut Vu) -> Direct<'a> {
        Direct {
            vu,
            gif: &mut self.gif,
            gs: &mut self.gs,
        }
    }

    /// Runs what is queued, in order: with a backend, waits for it, and
    /// brings VU1's state back into `vu` when it is wanted (VU1's memory
    /// read or written, a save); then gives what VU1 sent the GIF to `gif`
    /// and `gs`.
    pub fn run(&mut self, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs, why: Why) {
        let report = self.backend.is_some() && self.behind && matches!(why, Why::VuMemory | Why::Save);
        if self.is_empty() && !report {
            return;
        }
        self.stats.syncs[why as usize] += 1;
        self.stats.window += self.now - self.since;
        #[cfg(not(target_arch = "wasm32"))]
        let started = std::time::Instant::now();
        let records = match self.backend.as_mut() {
            None => self.gif.capture.as_mut().map(Stream::take).unwrap_or_default(),
            Some(_) => {
                if report {
                    self.ops.tag(op::REPORT);
                    self.behind = false;
                }
                self.send();
                self.sent = false;
                self.backend.as_mut().expect("a backend").wait()
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        let replaying = std::time::Instant::now();
        replay(&records, vu, gif, gs);
        // The buffer, for the next ones.
        if self.backend.is_none()
            && let Some(c) = self.gif.capture.as_mut()
        {
            c.reuse(records);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.stats.replay_ns += replaying.elapsed().as_nanos() as u64;
            self.stats.sync_ns += started.elapsed().as_nanos() as u64;
        }
    }

    fn enc_start(&mut self, pc: Option<u32>, itop: Option<u16>, top: Option<u16>) {
        self.begin();
        self.ops.tag(op::START);
        self.ops
            .buf
            .push(u8::from(pc.is_some()) | u8::from(itop.is_some()) << 1 | u8::from(top.is_some()) << 2);
        if let Some(pc) = pc {
            self.ops.put(&pc.to_le_bytes());
        }
        if let Some(itop) = itop {
            self.ops.put(&itop.to_le_bytes());
        }
        if let Some(top) = top {
            self.ops.put(&top.to_le_bytes());
        }
        self.end();
    }
}

impl Feed<'_> {
    /// The EE starts a program (CMSAR1).
    pub fn start_at(&mut self, pc: u32) {
        self.q.stats.starts += 1;
        if self.q.backend.is_some() {
            self.q.enc_start(Some(pc), None, None);
        } else {
            self.q.begin();
            let itop = self.vu.itop;
            self.q.here(self.vu).start(Some(pc), itop, None);
        }
    }

    /// FBRST: VU1 reset.
    pub fn reset(&mut self) {
        self.q.begin();
        if self.q.backend.is_some() {
            self.q.ops.tag(op::RESET);
            self.q.end();
        } else {
            self.vu.reset();
        }
    }
}

impl VuSink for Feed<'_> {
    // With VU1 elsewhere, VIF1's words went there (`Vu1Queue::vif1`): its
    // VIF1 does these.

    fn write_lanes(&mut self, qaddr: u32, lanes: [Option<u32>; 4]) {
        if self.q.backend.is_none() {
            self.q.begin();
            self.q.here(self.vu).write_lanes(qaddr, lanes);
        }
    }

    fn write_micro(&mut self, at: u32, word: u32) {
        if self.q.backend.is_none() {
            self.q.begin();
            self.q.here(self.vu).write_micro(at, word);
        }
    }

    fn values(&self) -> bool {
        self.q.backend.is_none()
    }

    fn micro_len(&self) -> u32 {
        MICRO
    }

    fn start(&mut self, pc: Option<u32>, itop: u16, top: Option<u16>) {
        self.q.stats.starts += 1;
        if self.q.backend.is_none() {
            self.q.begin();
            self.q.here(self.vu).start(pc, itop, top);
        }
    }

    fn direct(&mut self, q: u128) {
        if self.q.backend.is_none() {
            self.q.begin();
            self.q.here(self.vu).direct(q);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOP: u32 = 0x2FF;
    const LNOP: u32 = 0x8000_033C;
    const E: u32 = 1 << 30;

    fn code(cmd: u32, num: u32, imm: u32) -> u32 {
        (cmd << 24) | (num << 16) | imm
    }

    /// VIF1 code: a program (it kicks the packet at 0x10, then the one at
    /// VI1 + 2) loaded and started, data unpacked every which way, DIRECT
    /// data.
    fn stream() -> Vec<u32> {
        let iaddiu = |it: u32, imm: u32| (0x08 << 25) | (it << 16) | imm;
        let xgkick = |is: u32| (0x40 << 25) | (is << 11) | (0x1B << 6) | 0x3C;
        let prog = [
            (NOP, iaddiu(1, 0x10)),
            (NOP, xgkick(1)),
            (NOP, iaddiu(1, 0x12)),
            (NOP, xgkick(1)),
            (NOP | E, LNOP),
            (NOP, LNOP),
        ];
        let mut w = vec![code(0x4A, prog.len() as u32, 0)];
        for (upper, lower) in prog {
            w.extend([lower, upper]);
        }
        // At 0x10 and 0x12: packets of 1 loop, EOP, packed, A+D (to
        // FINISH), as V4-32.
        let tag = [1 | (1 << 15), 1 << 28, 0xE, 0];
        let ad = [7, 0, 0x61, 0];
        w.push(code(0x01, 0, 0x0101));
        w.push(code(0x6C, 4, 0x10));
        for q in [tag, ad, tag, ad] {
            w.extend(q);
        }
        // V3-16, masked (x from the data, y the row, z the column, w kept),
        // with the row added.
        w.extend([code(0x30, 0, 0), 1, 2, 3, 4, code(0x31, 0, 0), 5, 6, 7, 8]);
        w.extend([code(0x20, 0, 0), 0b11_10_01_00, code(0x05, 0, 1)]);
        w.push(code(0x79, 3, 0x40));
        w.extend([0x0002_0001, 0x0004_0003, 0x0006_0005, 0x0008_0007, 0x000A_0009]);
        // Filling (CL 1, WL 3), accumulating into the row: V2-8.
        w.extend([code(0x01, 0, 0x0301), code(0x05, 0, 2)]);
        w.push(code(0x66, 6, 0x50));
        w.push(0x0403_0201);
        // V4-5, back to normal.
        w.extend([code(0x01, 0, 0x0101), code(0x05, 0, 0)]);
        w.push(code(0x6F, 3, 0x60));
        w.extend([0x7FFF_1234, 0x0000_8421]);
        // Run it; DIRECT.
        w.push(code(0x14, 0, 0));
        w.push(code(0x50, 0, 2));
        w.extend(tag);
        w.extend([5, 0, 0x61, 0]);
        w
    }

    fn bytes<T: serde::Serialize>(v: &T) -> Vec<u8> {
        postcard::to_stdvec(v).unwrap()
    }

    /// The same VIF1 words, acted on here and by an engine in a thread
    /// (cut at odd places), leave VU1, VIF1, the GIF and the GS the same.
    #[test]
    fn engine_matches_inline() {
        let words = stream();
        let mut runs = Vec::new();
        for remote in [false, true] {
            let (mut vu, mut gif, mut gs, mut vif) = (Vu::new(1), Gif::default(), Gs::default(), Vif::new(1));
            let mut q = Vu1Queue::default();
            if remote {
                q.set_backend(Some(Box::new(Thread::spawn())), &vu, &gif, &vif);
            }
            for part in words.chunks(7) {
                q.vif1(&mut vif, &mut vu, part);
            }
            q.feed(&mut vu).start_at(0);
            q.run(&mut vu, &mut gif, &mut gs, Why::VBlank);
            // The EE side writes a word of VU1's data, then reads it all.
            q.run(&mut vu, &mut gif, &mut gs, Why::VuMemory);
            vu.data[0x400..0x404].copy_from_slice(&9u32.to_le_bytes());
            q.poke(0x4000 + 0x400, &9u32.to_le_bytes());
            for part in words.chunks(5) {
                q.vif1(&mut vif, &mut vu, part);
            }
            q.run(&mut vu, &mut gif, &mut gs, Why::Save);
            assert!(q.is_empty());
            assert_eq!(gif.qwords[0], 12, "each program kicked its two packets");
            assert_eq!(gif.qwords[1], 4);
            runs.push((vu.data.to_vec(), bytes(&vu), bytes(&gif), bytes(&gs), bytes(&vif)));
        }
        assert!(runs[0].0[0x400..0x700].iter().any(|&b| b != 0), "the unpacks wrote");
        assert_eq!(runs[0], runs[1]);
    }
}
