//! The PS2's memory card (8 MiB): pages of 512 bytes, each followed by 16
//! spare bytes holding its ECC; clusters of two pages; a FAT (reached
//! through an indirect table) chaining clusters; directories of 512-byte
//! entries. A save is a directory of the root, its title in its `icon.sys`.
//! The format is the one ps2sdk's MCMAN and the mymc tool document; the ECC
//! is a Hamming code over each 128-byte quarter of a page, as MCMAN checks
//! it on reading.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{CardError, CardInfo, Save, sjis};

const PAGE_DATA: usize = 512;
const PAGE: usize = PAGE_DATA + 16;
const PAGES: usize = 16_384;
pub const SIZE: usize = PAGE * PAGES;
const CLUSTER: usize = 2 * PAGE_DATA;
/// FAT entries (u32) in a cluster.
const PER_CLUSTER: usize = CLUSTER / 4;
const ENTRY: usize = 512;

const MAGIC: &[u8; 28] = b"Sony PS2 Memory Card Format ";

/// FAT entries: allocated (with the next cluster), the end of a chain, free.
const ALLOCATED: u32 = 0x8000_0000;
const CHAIN_END: u32 = 0xffff_ffff;
const FREE: u32 = 0x7fff_ffff;

/// Directory entry modes.
const DF_EXISTS: u16 = 0x8000;
const DF_DIR: u16 = 0x0020;

/// The layout of a standard 8 MiB card, as the console formats it.
const CLUSTERS: u32 = 8192;
const IFC: u32 = 8;
const FAT_FIRST: u32 = 9;
const FAT_CLUSTERS: u32 = 32;
const ALLOC_OFFSET: u32 = FAT_FIRST + FAT_CLUSTERS;
/// Two erase blocks (8 clusters each) at the end are backups.
const ALLOC_END: u32 = CLUSTERS - ALLOC_OFFSET - 16;

// --- Pages and ECC -----------------------------------------------------------

fn parity(b: u8) -> u8 {
    (b.count_ones() & 1) as u8
}

/// The 3 ECC bytes of 128 bytes of data.
fn ecc(chunk: &[u8]) -> [u8; 3] {
    const MASKS: [u8; 7] = [0x55, 0x33, 0x0f, 0x00, 0xaa, 0xcc, 0xf0];
    let mut column: u8 = 0x77;
    let mut line0: u8 = 0x7f;
    let mut line1: u8 = 0x7f;
    for (i, &b) in chunk.iter().enumerate() {
        let mask = MASKS
            .iter()
            .enumerate()
            .fold(0u8, |m, (k, &cm)| m | (parity(b & cm) << k));
        column ^= mask;
        if parity(b) == 1 {
            line0 ^= !(i as u8);
            line1 ^= i as u8;
        }
    }
    [column, line0 & 0x7f, line1]
}

fn write_page(data: &mut [u8], page: usize, bytes: &[u8]) {
    let at = page * PAGE;
    data[at..at + PAGE_DATA].copy_from_slice(bytes);
    let mut spare = [0u8; 16];
    for (k, chunk) in bytes.as_chunks::<128>().0.iter().enumerate() {
        spare[3 * k..3 * k + 3].copy_from_slice(&ecc(chunk));
    }
    data[at + PAGE_DATA..at + PAGE].copy_from_slice(&spare);
}

fn read_cluster(data: &[u8], abs: u32) -> Vec<u8> {
    let page = abs as usize * 2;
    let mut out = Vec::with_capacity(CLUSTER);
    for p in page..page + 2 {
        out.extend_from_slice(&data[p * PAGE..p * PAGE + PAGE_DATA]);
    }
    out
}

fn write_cluster(data: &mut [u8], abs: u32, bytes: &[u8]) {
    let page = abs as usize * 2;
    write_page(data, page, &bytes[..PAGE_DATA]);
    write_page(data, page + 1, &bytes[PAGE_DATA..CLUSTER]);
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

// --- The superblock and the FAT ----------------------------------------------

struct Layout {
    alloc_offset: u32,
    alloc_end: u32,
    root: u32,
    ifc: Vec<u32>,
}

fn layout(data: &[u8]) -> Result<Layout, CardError> {
    if data.len() != SIZE {
        return Err(CardError::Corrupt("not a PS2 memory card image"));
    }
    let sb = &data[..PAGE_DATA];
    if &sb[..28] != MAGIC {
        return Err(if sb.iter().all(|&b| b == 0xff) {
            CardError::Unformatted
        } else {
            CardError::Corrupt("no file system")
        });
    }
    let page_len = u16::from_le_bytes([sb[0x28], sb[0x29]]);
    let pages_per_cluster = u16::from_le_bytes([sb[0x2a], sb[0x2b]]);
    if page_len as usize != PAGE_DATA || pages_per_cluster != 2 {
        return Err(CardError::Corrupt("an unusual page or cluster size"));
    }
    let l = Layout {
        alloc_offset: u32_at(sb, 0x34),
        alloc_end: u32_at(sb, 0x38),
        root: u32_at(sb, 0x3c),
        ifc: (0..32).map(|k| u32_at(sb, 0x50 + 4 * k)).collect(),
    };
    if l.alloc_offset as usize + l.alloc_end as usize > SIZE / PAGE / 2 {
        return Err(CardError::Corrupt("its clusters exceed the card"));
    }
    Ok(l)
}

/// The FAT, loaded whole: an entry per allocatable cluster (relative to
/// `alloc_offset`), and the clusters holding it, to write it back.
struct Fat {
    entries: Vec<u32>,
    clusters: Vec<u32>,
}

impl Fat {
    fn load(data: &[u8], l: &Layout) -> Result<Self, CardError> {
        let needed = (l.alloc_end as usize).div_ceil(PER_CLUSTER);
        let mut clusters = Vec::with_capacity(needed);
        let mut entries = Vec::with_capacity(needed * PER_CLUSTER);
        for k in 0..needed {
            let ifc = *l
                .ifc
                .get(k / PER_CLUSTER)
                .ok_or(CardError::Corrupt("its FAT is too large"))?;
            let indirect = read_cluster(data, ifc);
            let fat = u32_at(&indirect, (k % PER_CLUSTER) * 4);
            if fat as usize >= SIZE / PAGE / 2 {
                return Err(CardError::Corrupt("its FAT is out of the card"));
            }
            let c = read_cluster(data, fat);
            entries.extend((0..PER_CLUSTER).map(|i| u32_at(&c, 4 * i)));
            clusters.push(fat);
        }
        Ok(Self { entries, clusters })
    }

    fn save(&self, data: &mut [u8]) {
        for (k, &fat) in self.clusters.iter().enumerate() {
            let bytes: Vec<u8> = self.entries[k * PER_CLUSTER..(k + 1) * PER_CLUSTER]
                .iter()
                .flat_map(|e| e.to_le_bytes())
                .collect();
            write_cluster(data, fat, &bytes);
        }
    }

    fn chain(&self, first: u32) -> Result<Vec<u32>, CardError> {
        let mut out = Vec::new();
        if first == CHAIN_END {
            return Ok(out);
        }
        let mut c = first;
        loop {
            let e = *self
                .entries
                .get(c as usize)
                .ok_or(CardError::Corrupt("a chain leaves the card"))?;
            if e & ALLOCATED == 0 || out.len() > self.entries.len() {
                return Err(CardError::Corrupt("a broken cluster chain"));
            }
            out.push(c);
            if e == CHAIN_END {
                return Ok(out);
            }
            c = e & !ALLOCATED;
        }
    }

    fn free(&self, alloc_end: u32) -> impl Iterator<Item = u32> + '_ {
        (0..alloc_end).filter(|&c| self.entries.get(c as usize).is_some_and(|e| e & ALLOCATED == 0))
    }

    /// Takes `n` free clusters, chained in order.
    fn allocate(&mut self, alloc_end: u32, n: usize) -> Result<Vec<u32>, CardError> {
        let got: Vec<u32> = self.free(alloc_end).take(n).collect();
        if got.len() < n {
            return Err(CardError::Full);
        }
        self.link(&got);
        Ok(got)
    }

    fn link(&mut self, clusters: &[u32]) {
        for (k, &c) in clusters.iter().enumerate() {
            self.entries[c as usize] = clusters.get(k + 1).map_or(CHAIN_END, |n| ALLOCATED | n);
        }
    }

    fn release(&mut self, clusters: &[u32]) {
        for &c in clusters {
            self.entries[c as usize] = FREE;
        }
    }
}

// --- Directories -------------------------------------------------------------

#[derive(Clone)]
struct Entry([u8; ENTRY]);

impl Entry {
    fn mode(&self) -> u16 {
        u16::from_le_bytes([self.0[0], self.0[1]])
    }
    fn set_mode(&mut self, m: u16) {
        self.0[..2].copy_from_slice(&m.to_le_bytes());
    }
    fn exists(&self) -> bool {
        self.mode() & DF_EXISTS != 0
    }
    fn is_dir(&self) -> bool {
        self.mode() & DF_DIR != 0
    }
    fn length(&self) -> u32 {
        u32_at(&self.0, 4)
    }
    fn set_length(&mut self, n: u32) {
        self.0[4..8].copy_from_slice(&n.to_le_bytes());
    }
    fn cluster(&self) -> u32 {
        u32_at(&self.0, 0x10)
    }
    fn set_cluster(&mut self, c: u32) {
        self.0[0x10..0x14].copy_from_slice(&c.to_le_bytes());
    }
    fn set_parent_entry(&mut self, n: u32) {
        self.0[0x14..0x18].copy_from_slice(&n.to_le_bytes());
    }
    fn name(&self) -> String {
        let raw = &self.0[0x40..0x60];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..end]).into_owned()
    }
}

/// A directory's entries (its "." first, giving their number), and its
/// clusters.
fn read_dir(data: &[u8], l: &Layout, fat: &Fat, first: u32) -> Result<(Vec<Entry>, Vec<u32>), CardError> {
    let clusters = fat.chain(first)?;
    let mut entries = Vec::new();
    for &c in &clusters {
        let bytes = read_cluster(data, c + l.alloc_offset);
        for half in bytes.as_chunks::<ENTRY>().0 {
            entries.push(Entry(*half));
        }
    }
    let count = entries.first().map_or(0, |dot| dot.length() as usize);
    if count < 2 || count > entries.len() {
        return Err(CardError::Corrupt("a directory's size"));
    }
    entries.truncate(count);
    Ok((entries, clusters))
}

fn write_dir(data: &mut [u8], l: &Layout, clusters: &[u32], entries: &[Entry]) {
    for (k, &c) in clusters.iter().enumerate() {
        let mut bytes = vec![0u8; CLUSTER];
        for (h, half) in bytes.as_chunks_mut::<ENTRY>().0.iter_mut().enumerate() {
            if let Some(e) = entries.get(2 * k + h) {
                half.copy_from_slice(&e.0);
            }
        }
        write_cluster(data, c + l.alloc_offset, &bytes);
    }
}

fn read_file(data: &[u8], l: &Layout, fat: &Fat, e: &Entry) -> Result<Vec<u8>, CardError> {
    let mut out = Vec::new();
    for c in fat.chain(e.cluster())? {
        out.extend_from_slice(&read_cluster(data, c + l.alloc_offset));
    }
    let len = e.length() as usize;
    if len > out.len() {
        return Err(CardError::Corrupt("a file longer than its clusters"));
    }
    out.truncate(len);
    Ok(out)
}

/// The save directory `id` in the root: its index there and entry.
fn find(root: &[Entry], id: &str) -> Option<(usize, Entry)> {
    root.iter()
        .enumerate()
        .skip(2)
        .find(|(_, e)| e.exists() && e.is_dir() && e.name() == id)
        .map(|(k, e)| (k, e.clone()))
}

/// The title in an `icon.sys`: two lines, the second starting at the offset
/// at 6.
fn title(icon_sys: &[u8]) -> Option<String> {
    if icon_sys.len() < 0x104 || &icon_sys[..4] != b"PS2D" {
        return None;
    }
    let raw = &icon_sys[0xc0..0x104];
    let split = usize::from(u16::from_le_bytes([icon_sys[6], icon_sys[7]])).min(raw.len());
    let first = sjis::decode(&raw[..split]);
    let second = sjis::decode(&raw[split..]);
    Some(format!("{} {}", first.trim(), second.trim()).trim().to_owned())
}

fn clusters_for(bytes: u32) -> u32 {
    bytes.div_ceil(CLUSTER as u32)
}

// --- Operations --------------------------------------------------------------

pub fn list(data: &[u8]) -> Result<CardInfo, CardError> {
    let l = match layout(data) {
        Err(CardError::Unformatted) => {
            return Ok(CardInfo {
                formatted: false,
                saves: Vec::new(),
                free: 0,
                capacity: 0,
            });
        }
        other => other?,
    };
    let fat = Fat::load(data, &l)?;
    let (root, _) = read_dir(data, &l, &fat, l.root)?;
    let mut saves = Vec::new();
    for e in root.iter().skip(2).filter(|e| e.exists() && e.is_dir()) {
        let (files, dir_clusters) = read_dir(data, &l, &fat, e.cluster())?;
        let mut size = dir_clusters.len() as u32;
        let mut save_title = None;
        for f in files.iter().skip(2).filter(|f| f.exists()) {
            size += clusters_for(f.length());
            if f.name() == "icon.sys" && !f.is_dir() {
                save_title = title(&read_file(data, &l, &fat, f)?);
            }
        }
        saves.push(Save {
            id: e.name(),
            title: save_title.unwrap_or_default(),
            size: size * CLUSTER as u32,
            icon: Vec::new(),
        });
    }
    let free = fat.free(l.alloc_end).count() as u32;
    Ok(CardInfo {
        formatted: true,
        saves,
        free: free * CLUSTER as u32,
        capacity: l.alloc_end * CLUSTER as u32,
    })
}

pub fn delete(data: &mut [u8], id: &str) -> Result<(), CardError> {
    let l = layout(data)?;
    let mut fat = Fat::load(data, &l)?;
    let (mut root, root_clusters) = read_dir(data, &l, &fat, l.root)?;
    let (index, dir) = find(&root, id).ok_or(CardError::NoSuchSave)?;
    let (files, dir_clusters) = read_dir(data, &l, &fat, dir.cluster())?;
    for f in files.iter().skip(2).filter(|f| f.exists()) {
        let chain = fat.chain(f.cluster())?;
        fat.release(&chain);
    }
    fat.release(&dir_clusters);
    let mode = root[index].mode();
    root[index].set_mode(mode & !DF_EXISTS);
    write_dir(data, &l, &root_clusters, &root);
    fat.save(data);
    Ok(())
}

pub fn copy(from: &[u8], id: &str, to: &mut [u8]) -> Result<(), CardError> {
    let fl = layout(from)?;
    let ffat = Fat::load(from, &fl)?;
    let (froot, _) = read_dir(from, &fl, &ffat, fl.root)?;
    let (_, dir) = find(&froot, id).ok_or(CardError::NoSuchSave)?;
    let (entries, _) = read_dir(from, &fl, &ffat, dir.cluster())?;
    let mut files = Vec::new();
    for e in entries.iter().skip(2).filter(|e| e.exists()) {
        if e.is_dir() {
            return Err(CardError::Corrupt("a save with directories inside"));
        }
        files.push((e.clone(), read_file(from, &fl, &ffat, e)?));
    }

    let l = layout(to)?;
    let mut fat = Fat::load(to, &l)?;
    let (mut root, mut root_clusters) = read_dir(to, &l, &fat, l.root)?;
    if find(&root, id).is_some() {
        return Err(CardError::Exists);
    }
    let slot = root_slot(&mut fat, &l, &mut root, &mut root_clusters)?;
    // The save's directory: ".", "..", its files.
    let count = entries.len() as u32;
    let dir_clusters = fat.allocate(l.alloc_end, clusters_for(count * ENTRY as u32) as usize)?;
    let mut new_entries = vec![entries[0].clone(), entries[1].clone()];
    new_entries[0].set_cluster(dir_clusters[0]);
    new_entries[0].set_length(2 + files.len() as u32);
    new_entries[0].set_parent_entry(slot as u32);
    new_entries[1].set_cluster(0);
    new_entries[1].set_parent_entry(0);
    for (e, bytes) in &files {
        let mut e = e.clone();
        let n = clusters_for(bytes.len() as u32) as usize;
        if n == 0 {
            e.set_cluster(CHAIN_END);
        } else {
            let clusters = fat.allocate(l.alloc_end, n)?;
            for (k, &c) in clusters.iter().enumerate() {
                let mut chunk = bytes[k * CLUSTER..bytes.len().min((k + 1) * CLUSTER)].to_vec();
                chunk.resize(CLUSTER, 0xff);
                write_cluster(to, c + l.alloc_offset, &chunk);
            }
            e.set_cluster(clusters[0]);
        }
        e.set_parent_entry(0);
        new_entries.push(e);
    }
    write_dir(to, &l, &dir_clusters, &new_entries);
    let mut entry = dir.clone();
    entry.set_cluster(dir_clusters[0]);
    entry.set_length(2 + files.len() as u32);
    entry.set_parent_entry(0);
    root[slot] = entry;
    write_dir(to, &l, &root_clusters, &root);
    fat.save(to);
    Ok(())
}

/// Room for an entry in the root: a deleted entry, or one more (in a new
/// cluster when the root's are full).
fn root_slot(fat: &mut Fat, l: &Layout, root: &mut Vec<Entry>, clusters: &mut Vec<u32>) -> Result<usize, CardError> {
    if let Some(k) = root.iter().skip(2).position(|e| !e.exists()) {
        return Ok(k + 2);
    }
    if root.len() == clusters.len() * 2 {
        let extra = fat.allocate(l.alloc_end, 1)?[0];
        clusters.push(extra);
        fat.link(clusters);
    }
    root.push(Entry([0; ENTRY]));
    let n = root.len() as u32;
    root[0].set_length(n);
    Ok(n as usize - 1)
}

/// A directory entry's time (the card's: seconds, minutes, hours, day,
/// month, year), now, in UTC.
fn now() -> [u8; 8] {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    let y = year.to_le_bytes();
    [
        0,
        (rem % 60) as u8,
        (rem / 60 % 60) as u8,
        (rem / 3600) as u8,
        day,
        month,
        y[0],
        y[1],
    ]
}

fn dir_entry(mode: u16, length: u32, name: &str, time: [u8; 8]) -> Entry {
    let mut e = Entry([0; ENTRY]);
    e.set_mode(mode);
    e.set_length(length);
    e.0[0x08..0x10].copy_from_slice(&time);
    e.0[0x18..0x20].copy_from_slice(&time);
    e.0[0x40..0x40 + name.len()].copy_from_slice(name.as_bytes());
    e
}

/// A formatted, empty card, laid out as the console formats one.
pub fn formatted() -> Vec<u8> {
    let mut data = vec![0xffu8; SIZE];
    let mut sb = vec![0u8; CLUSTER];
    sb[..28].copy_from_slice(MAGIC);
    sb[0x1c..0x23].copy_from_slice(b"1.2.0.0");
    sb[0x28..0x2a].copy_from_slice(&(PAGE_DATA as u16).to_le_bytes());
    sb[0x2a..0x2c].copy_from_slice(&2u16.to_le_bytes());
    sb[0x2c..0x2e].copy_from_slice(&16u16.to_le_bytes());
    sb[0x2e..0x30].copy_from_slice(&0xff00u16.to_le_bytes());
    for (at, v) in [
        (0x30, CLUSTERS),
        (0x34, ALLOC_OFFSET),
        (0x38, ALLOC_END),
        (0x3c, 0),
        // The backup erase blocks: the last two.
        (0x40, 1023),
        (0x44, 1022),
        (0x50, IFC),
    ] {
        sb[at..at + 4].copy_from_slice(&u32::to_le_bytes(v));
    }
    // No bad blocks.
    sb[0xd0..0x150].fill(0xff);
    // Card type (PS2), flags: ECC, bad blocks list, erased to 1s... as
    // formatted cards have them.
    sb[0x150] = 2;
    sb[0x151] = 0x52;
    write_cluster(&mut data, 0, &sb);
    // The indirect FAT cluster, then the FAT.
    let ifc: Vec<u8> = (0..PER_CLUSTER as u32)
        .flat_map(|k| if k < FAT_CLUSTERS { FAT_FIRST + k } else { 0 }.to_le_bytes())
        .collect();
    write_cluster(&mut data, IFC, &ifc);
    let mut fat = Fat {
        entries: vec![FREE; FAT_CLUSTERS as usize * PER_CLUSTER],
        clusters: (FAT_FIRST..FAT_FIRST + FAT_CLUSTERS).collect(),
    };
    // The root directory, in the first cluster.
    fat.entries[0] = CHAIN_END;
    fat.save(&mut data);
    let time = now();
    let root = [dir_entry(0x8427, 2, ".", time), dir_entry(0xa426, 0, "..", time)];
    let layout = Layout {
        alloc_offset: ALLOC_OFFSET,
        alloc_end: ALLOC_END,
        root: 0,
        ifc: vec![IFC],
    };
    write_dir(&mut data, &layout, &[0], &root);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adds save `id` with files of these sizes (an icon.sys titled "ＡＢ"
    /// first), by hand, as a console would write it.
    fn add_save(data: &mut [u8], id: &str, sizes: &[usize]) {
        let l = layout(data).unwrap();
        let mut fat = Fat::load(data, &l).unwrap();
        let (mut root, mut root_clusters) = read_dir(data, &l, &fat, l.root).unwrap();
        let slot = root_slot(&mut fat, &l, &mut root, &mut root_clusters).unwrap();
        let time = now();
        let mut entries = vec![dir_entry(0x8427, 0, ".", time), dir_entry(0xa426, 0, "..", time)];
        let mut icon = vec![0u8; 964];
        icon[..4].copy_from_slice(b"PS2D");
        icon[6] = 2;
        icon[0xc0..0xc4].copy_from_slice(&[0x82, 0x60, 0x82, 0x61]);
        let mut contents: Vec<(String, Vec<u8>)> = vec![("icon.sys".to_owned(), icon)];
        for (k, &n) in sizes.iter().enumerate() {
            contents.push((format!("file{k}"), (0..n).map(|i| (i * 7 + k) as u8).collect()));
        }
        for (name, bytes) in &contents {
            let mut e = dir_entry(0x8417, bytes.len() as u32, name, time);
            let n = clusters_for(bytes.len() as u32) as usize;
            if n == 0 {
                e.set_cluster(CHAIN_END);
            } else {
                let cs = fat.allocate(l.alloc_end, n).unwrap();
                for (k, &c) in cs.iter().enumerate() {
                    let mut chunk = bytes[k * CLUSTER..bytes.len().min((k + 1) * CLUSTER)].to_vec();
                    chunk.resize(CLUSTER, 0xff);
                    write_cluster(data, c + l.alloc_offset, &chunk);
                }
                e.set_cluster(cs[0]);
            }
            entries.push(e);
        }
        let n = entries.len() as u32;
        entries[0].set_length(n);
        let dc = fat
            .allocate(l.alloc_end, clusters_for(n * ENTRY as u32) as usize)
            .unwrap();
        entries[0].set_cluster(dc[0]);
        write_dir(data, &l, &dc, &entries);
        let mut e = dir_entry(0x8427, n, id, time);
        e.set_cluster(dc[0]);
        root[slot] = e;
        write_dir(data, &l, &root_clusters, &root);
        fat.save(data);
    }

    fn file(data: &[u8], id: &str, name: &str) -> Vec<u8> {
        let l = layout(data).unwrap();
        let fat = Fat::load(data, &l).unwrap();
        let (root, _) = read_dir(data, &l, &fat, l.root).unwrap();
        let (_, dir) = find(&root, id).unwrap();
        let (entries, _) = read_dir(data, &l, &fat, dir.cluster()).unwrap();
        let e = entries.iter().find(|e| e.name() == name).unwrap();
        read_file(data, &l, &fat, e).unwrap()
    }

    #[test]
    fn ecc_of_known_chunks() {
        // All zeros: no parity anywhere.
        assert_eq!(ecc(&[0u8; 128]), [0x77, 0x7f, 0x7f]);
        // One bit set in byte 5: the line parities take its index.
        let mut c = [0u8; 128];
        c[5] = 1;
        let [col, l0, l1] = ecc(&c);
        assert_eq!(l1, 0x7f ^ 5);
        assert_eq!(l0, (0x7f ^ !5u8) & 0x7f);
        // Bit 0 is in the masks 0x55, 0x33 and 0x0F: column bits 0 to 2.
        assert_eq!(col, 0x77 ^ 0x07);
    }

    #[test]
    fn a_formatted_card_is_empty() {
        let card = formatted();
        let info = list(&card).unwrap();
        assert!(info.formatted);
        assert!(info.saves.is_empty());
        assert_eq!(info.capacity, ALLOC_END * CLUSTER as u32);
        // The root's cluster is taken.
        assert_eq!(info.free, (ALLOC_END - 1) * CLUSTER as u32);
        assert!(!list(&vec![0xff; SIZE]).unwrap().formatted);
    }

    #[test]
    fn copies_then_deletes() {
        let mut from = formatted();
        add_save(&mut from, "BESLES-12345", &[3000, 0, 1024]);
        let info = list(&from).unwrap();
        assert_eq!(info.saves[0].title, "A B");
        // icon.sys (1), 3000 bytes (3), 0 (0), 1024 (1), and the directory
        // (6 entries: 3 clusters).
        assert_eq!(info.saves[0].size, 8 * 1024);

        let mut to = formatted();
        add_save(&mut to, "BASLUS-99999", &[100]);
        copy(&from, "BESLES-12345", &mut to).unwrap();
        let info = list(&to).unwrap();
        assert_eq!(info.saves.len(), 2);
        for (k, n) in [3000usize, 0, 1024].iter().enumerate() {
            let expected: Vec<u8> = (0..*n).map(|i| (i * 7 + k) as u8).collect();
            assert_eq!(file(&to, "BESLES-12345", &format!("file{k}")), expected);
        }
        assert_eq!(copy(&from, "BESLES-12345", &mut to), Err(CardError::Exists));

        let before = list(&to).unwrap().free;
        delete(&mut to, "BESLES-12345").unwrap();
        let info = list(&to).unwrap();
        assert_eq!(info.saves.len(), 1);
        assert_eq!(info.free, before + 8 * 1024);
        // Its root entry is reused by the next copy.
        copy(&from, "BESLES-12345", &mut to).unwrap();
        assert_eq!(list(&to).unwrap().saves.len(), 2);
    }

    #[test]
    fn every_written_page_has_its_ecc() {
        let card = formatted();
        for page in [
            0usize,
            2 * IFC as usize,
            2 * FAT_FIRST as usize,
            2 * ALLOC_OFFSET as usize,
        ] {
            let at = page * PAGE;
            for k in 0..4 {
                let chunk = &card[at + 128 * k..at + 128 * (k + 1)];
                assert_eq!(&card[at + PAGE_DATA + 3 * k..at + PAGE_DATA + 3 * k + 3], &ecc(chunk));
            }
        }
    }

    /// Checks against the console: `CARD_OUT=card.ps2` writes a formatted
    /// card, for the PS2 example's `MC1`.
    #[test]
    #[ignore = "writes a file"]
    fn write_formatted_card() {
        let path = std::env::var("CARD_OUT").expect("CARD_OUT");
        std::fs::write(path, formatted()).unwrap();
    }

    /// `CARD_IN=card.ps2`: a card the console wrote; its pages' ECC must be
    /// what `ecc` computes, and its saves listed.
    #[test]
    #[ignore = "reads a file"]
    fn read_console_card() {
        let card = std::fs::read(std::env::var("CARD_IN").expect("CARD_IN")).unwrap();
        let mut checked = 0;
        for page in 0..PAGES {
            let at = page * PAGE;
            let spare = &card[at + PAGE_DATA..at + PAGE];
            if spare.iter().all(|&b| b == 0xff) {
                continue;
            }
            for k in 0..4 {
                let chunk = &card[at + 128 * k..at + 128 * (k + 1)];
                assert_eq!(&spare[3 * k..3 * k + 3], &ecc(chunk), "page {page}, quarter {k}");
            }
            checked += 1;
        }
        println!(
            "{checked} pages with ECC, all matching; superblock: {:02x?}",
            &card[..0x160]
        );
        println!("{:#?}", list(&card).unwrap());
    }

    #[test]
    fn dates() {
        let t = now();
        let year = u16::from_le_bytes([t[6], t[7]]);
        assert!((2026..2200).contains(&year));
        assert!((1..=12).contains(&t[5]) && (1..=31).contains(&t[4]));
    }
}
