//! The PlayStation's memory card: 16 blocks of 8 KiB. Block 0 holds the
//! header and a directory frame (128 bytes) for each of the other 15; a save
//! is a chain of blocks, its first block starting with its title and icon.
//! Layout from psx-spx (problemkaputt.de/psx-spx.htm, "Memory Card Data
//! Format").

use base64::Engine;

use super::{CardError, CardInfo, Save, sjis};

pub const SIZE: usize = 128 * 1024;
const BLOCK: usize = 8 * 1024;
const FRAME: usize = 128;
const BLOCKS: usize = 15;

/// Directory entry states.
const FREE: u32 = 0xa0;
const FIRST: u32 = 0x51;
const MIDDLE: u32 = 0x52;
const LAST: u32 = 0x53;
/// Deleted entries (0xA1-0xA3) are free too.
const fn is_free(state: u32) -> bool {
    matches!(state, 0xa0..=0xa3)
}

const NO_NEXT: u16 = 0xffff;

/// A formatted, empty card.
pub fn formatted() -> Vec<u8> {
    let mut data = vec![0u8; SIZE];
    data[..2].copy_from_slice(b"MC");
    checksum(&mut data, 0);
    for block in 1..=BLOCKS {
        free_entry(&mut data, block);
    }
    // The broken-sector list: none.
    for n in 16..36 {
        let at = n * FRAME;
        data[at..at + 4].fill(0xff);
        data[at + 8..at + 10].fill(0xff);
        checksum(&mut data, n);
    }
    data[63 * FRAME..63 * FRAME + 2].copy_from_slice(b"MC");
    checksum(&mut data, 63);
    data
}

fn frame(data: &[u8], n: usize) -> &[u8] {
    &data[n * FRAME..(n + 1) * FRAME]
}

fn checksum(data: &mut [u8], n: usize) {
    let f = &mut data[n * FRAME..(n + 1) * FRAME];
    f[127] = f[..127].iter().fold(0, |a, b| a ^ b);
}

fn state(data: &[u8], block: usize) -> u32 {
    u32::from_le_bytes(frame(data, block)[..4].try_into().expect("4 bytes"))
}

fn next(data: &[u8], block: usize) -> u16 {
    u16::from_le_bytes(frame(data, block)[8..10].try_into().expect("2 bytes"))
}

fn name(data: &[u8], block: usize) -> String {
    let raw = &frame(data, block)[0x0a..0x1e];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

fn free_entry(data: &mut [u8], block: usize) {
    let at = block * FRAME;
    data[at..at + FRAME].fill(0);
    data[at] = FREE as u8;
    data[at + 8..at + 10].copy_from_slice(&NO_NEXT.to_le_bytes());
    checksum(data, block);
}

fn check(data: &[u8]) -> Result<(), CardError> {
    if data.len() != SIZE {
        return Err(CardError::Corrupt("not a PlayStation memory card image"));
    }
    if &data[..2] != b"MC" {
        return Err(CardError::Corrupt("no card header"));
    }
    Ok(())
}

/// The blocks of the save starting at `first`, in order.
fn chain(data: &[u8], first: usize) -> Result<Vec<usize>, CardError> {
    let mut blocks = vec![first];
    let mut block = first;
    loop {
        let n = next(data, block);
        if n == NO_NEXT {
            break;
        }
        block = usize::from(n) + 1;
        if !(1..=BLOCKS).contains(&block) || blocks.contains(&block) {
            return Err(CardError::Corrupt("a save's blocks are not linked properly"));
        }
        blocks.push(block);
    }
    Ok(blocks)
}

/// The first blocks of the saves.
fn firsts(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    (1..=BLOCKS).filter(|&b| state(data, b) == FIRST)
}

fn find(data: &[u8], id: &str) -> Option<usize> {
    firsts(data).find(|&b| name(data, b) == id)
}

pub fn list(data: &[u8]) -> Result<CardInfo, CardError> {
    check(data)?;
    let mut saves = Vec::new();
    for first in firsts(data) {
        let blocks = chain(data, first)?;
        let start = first * BLOCK;
        let header = &data[start..start + FRAME];
        let (title, icon) = if &header[..2] == b"SC" {
            (sjis::decode(&header[4..0x44]).trim().to_owned(), icon(data, start))
        } else {
            (String::new(), Vec::new())
        };
        saves.push(Save {
            id: name(data, first),
            title,
            size: (blocks.len() * BLOCK) as u32,
            icon,
        });
    }
    let free = (1..=BLOCKS).filter(|&b| is_free(state(data, b))).count();
    Ok(CardInfo {
        formatted: true,
        saves,
        free: (free * BLOCK) as u32,
        capacity: (BLOCKS * BLOCK) as u32,
    })
}

/// The icon of the save starting at `start`: its frames (1 to 3), 16×16
/// RGBA, 4 bits per pixel through the palette of 15-bit colours (0:
/// transparent).
fn icon(data: &[u8], start: usize) -> Vec<String> {
    let header = &data[start..start + FRAME];
    let frames = match header[2] {
        0x11 => 1,
        0x12 => 2,
        0x13 => 3,
        _ => return Vec::new(),
    };
    let palette: Vec<[u8; 4]> = header[0x60..0x80]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            let v = u16::from_le_bytes(*c);
            let five = |s: u16| (((v >> s) & 0x1f) as u8) << 3 | (((v >> s) & 0x1f) as u8) >> 2;
            [five(0), five(5), five(10), if v == 0 { 0 } else { 255 }]
        })
        .collect();
    (0..frames)
        .map(|k| {
            let bitmap = &data[start + (1 + k) * FRAME..start + (2 + k) * FRAME];
            let rgba: Vec<u8> = bitmap
                .iter()
                .flat_map(|&b| [palette[usize::from(b & 0xf)], palette[usize::from(b >> 4)]])
                .flatten()
                .collect();
            base64::engine::general_purpose::STANDARD.encode(rgba)
        })
        .collect()
}

pub fn delete(data: &mut [u8], id: &str) -> Result<(), CardError> {
    check(data)?;
    let first = find(data, id).ok_or(CardError::NoSuchSave)?;
    for block in chain(data, first)? {
        free_entry(data, block);
    }
    Ok(())
}

pub fn copy(from: &[u8], id: &str, to: &mut [u8]) -> Result<(), CardError> {
    check(from)?;
    check(to)?;
    let first = find(from, id).ok_or(CardError::NoSuchSave)?;
    let blocks = chain(from, first)?;
    if find(to, id).is_some() {
        return Err(CardError::Exists);
    }
    let free: Vec<usize> = (1..=BLOCKS)
        .filter(|&b| is_free(state(to, b)))
        .take(blocks.len())
        .collect();
    if free.len() < blocks.len() {
        return Err(CardError::Full);
    }
    for (k, (&src, &dst)) in blocks.iter().zip(&free).enumerate() {
        to[dst * BLOCK..(dst + 1) * BLOCK].copy_from_slice(&from[src * BLOCK..(src + 1) * BLOCK]);
        // The directory entry: the first one as it was (its size and name),
        // the others cleared; then the state and the link.
        let at = dst * FRAME;
        if k == 0 {
            to[at..at + FRAME].copy_from_slice(frame(from, src));
        } else {
            to[at..at + FRAME].fill(0);
        }
        let last = k + 1 == blocks.len();
        let state = match (k, last) {
            (0, _) => FIRST,
            (_, true) => LAST,
            _ => MIDDLE,
        };
        to[at..at + 4].copy_from_slice(&state.to_le_bytes());
        let link = if last { NO_NEXT } else { (free[k + 1] - 1) as u16 };
        to[at + 8..at + 10].copy_from_slice(&link.to_le_bytes());
        checksum(to, dst);
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A card with a save of `blocks` blocks named `id`, titled "ＡＢ".
    pub fn with_save(id: &str, blocks: usize) -> Vec<u8> {
        let mut data = formatted();
        let free: Vec<usize> = (1..=BLOCKS)
            .filter(|&b| is_free(state(&data, b)))
            .take(blocks)
            .collect();
        for (k, &b) in free.iter().enumerate() {
            let start = b * BLOCK;
            data[start..start + BLOCK].fill(k as u8 + 1);
            if k == 0 {
                data[start..start + FRAME].fill(0);
                data[start..start + 2].copy_from_slice(b"SC");
                data[start + 2] = 0x11;
                data[start + 4..start + 8].copy_from_slice(&[0x82, 0x60, 0x82, 0x61]);
                // Colour 1: pure red.
                data[start + 0x62..start + 0x64].copy_from_slice(&0x001fu16.to_le_bytes());
                data[start + FRAME] = 0x10;
            }
            let at = b * FRAME;
            let state = if k == 0 {
                FIRST
            } else if k + 1 == blocks {
                LAST
            } else {
                MIDDLE
            };
            data[at..at + 4].copy_from_slice(&state.to_le_bytes());
            if k == 0 {
                data[at + 4..at + 8].copy_from_slice(&((blocks * BLOCK) as u32).to_le_bytes());
                data[at + 0x0a..at + 0x0a + id.len()].copy_from_slice(id.as_bytes());
            }
            let link = free.get(k + 1).map_or(NO_NEXT, |n| (n - 1) as u16);
            data[at + 8..at + 10].copy_from_slice(&link.to_le_bytes());
            checksum(&mut data, b);
        }
        data
    }

    #[test]
    fn lists_a_save_with_its_title_and_icon() {
        let card = with_save("BESLES-00049TEKKEN3", 2);
        let info = list(&card).unwrap();
        assert_eq!(info.saves.len(), 1);
        let save = &info.saves[0];
        assert_eq!(save.id, "BESLES-00049TEKKEN3");
        assert_eq!(save.title, "AB");
        assert_eq!(save.size, 2 * 8192);
        assert_eq!(info.free, 13 * 8192);
        let icon = base64::engine::general_purpose::STANDARD.decode(&save.icon[0]).unwrap();
        assert_eq!(icon.len(), 16 * 16 * 4);
        // Pixel 0: colour 0 (transparent); pixel 1: colour 1, red.
        assert_eq!(&icon[..8], &[0, 0, 0, 0, 255, 0, 0, 255]);
    }

    #[test]
    fn copies_then_deletes() {
        let from = with_save("BESLES-00049TEKKEN3", 3);
        // The destination's first blocks are taken: the copy goes elsewhere.
        let mut to = with_save("BESCES-00344", 2);
        copy(&from, "BESLES-00049TEKKEN3", &mut to).unwrap();
        let info = list(&to).unwrap();
        assert_eq!(info.saves.len(), 2);
        assert_eq!(info.free, 10 * 8192);
        // The data came along, block by block, in order.
        let first = find(&to, "BESLES-00049TEKKEN3").unwrap();
        let blocks = chain(&to, first).unwrap();
        assert_eq!(blocks, vec![3, 4, 5]);
        assert_eq!(to[4 * BLOCK + 100], 2);
        assert_eq!(to[5 * BLOCK + 100], 3);
        // Checksums hold.
        for n in 1..=BLOCKS {
            assert_eq!(frame(&to, n)[..127].iter().fold(0, |a, b| a ^ b), frame(&to, n)[127]);
        }
        assert_eq!(copy(&from, "BESLES-00049TEKKEN3", &mut to), Err(CardError::Exists));
        delete(&mut to, "BESLES-00049TEKKEN3").unwrap();
        let info = list(&to).unwrap();
        assert_eq!(info.saves.len(), 1);
        assert_eq!(info.free, 13 * 8192);
        assert_eq!(delete(&mut to, "BESLES-00049TEKKEN3"), Err(CardError::NoSuchSave));
    }

    #[test]
    fn a_full_card_refuses() {
        let from = with_save("BIG", 15);
        let mut to = with_save("SMALL", 1);
        assert_eq!(copy(&from, "BIG", &mut to), Err(CardError::Full));
    }
}
