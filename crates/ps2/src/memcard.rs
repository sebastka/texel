//! A PlayStation 2 memory card (8 MiB), as it answers on SIO2: pages of 512
//! bytes and 16 of ECC (kept as the IOP writes them), erased by blocks of
//! 16 pages, read and written in pieces of up to 128 bytes. Commands are
//! from ps2tek's memory card page (israpps.github.io/ps2tek); where replies
//! put each byte, from what ps2sdk's open-source MCMAN and SECRMAN check.
//!
//! The card's side of MagicGate (the authentication SECRMAN runs with the
//! drive's MechaCon) answers with fixed data: the MechaCon, which would
//! check it, accepts it (see cdvd.rs).

/// Bytes per page (data and ECC), pages per erase block, pages.
pub const PAGE: usize = 512 + 16;
pub const BLOCK_PAGES: usize = 16;
pub const PAGES: usize = 16_384;
/// A card's image: every page, ECC included.
pub const SIZE: usize = PAGE * PAGES;

/// The acknowledgement in every reply, and the terminator at reset.
const ACK: u8 = 0x2B;
const RESET_TERMINATOR: u8 = 0x55;

#[derive(Clone)]
pub struct MemoryCard {
    /// Every page, ECC included (erased: 0xFF).
    pub data: Vec<u8>,
    terminator: u8,
    /// The page the next read, write or erase is at, and the offset in it.
    page: usize,
    offset: usize,
    /// Written since the last `take_written`.
    written: bool,
}

impl MemoryCard {
    /// A card with this image (`SIZE` bytes; a shorter one is padded with
    /// erased pages), or a fresh, unformatted card.
    pub fn new(image: Option<&[u8]>) -> Self {
        let mut data = vec![0xFF; SIZE];
        if let Some(image) = image {
            let n = image.len().min(SIZE);
            data[..n].copy_from_slice(&image[..n]);
        }
        Self {
            data,
            terminator: RESET_TERMINATOR,
            page: 0,
            offset: 0,
            written: false,
        }
    }

    /// The card's image, if it was written since the last call.
    pub fn take_written(&mut self) -> Option<&[u8]> {
        std::mem::take(&mut self.written).then_some(&self.data[..])
    }

    /// One command (`packet`: the address byte 0x81, the command, its
    /// parameters): the bytes the card answers, one for each sent.
    pub fn exchange(&mut self, packet: &[u8]) -> Vec<u8> {
        let mut reply = vec![0xFF; packet.len()];
        if packet.len() < 2 {
            return reply;
        }
        // The reply to the command and its parameters, after the address.
        let r = &mut reply[1..];
        let n = r.len();
        let arg = |k: usize| packet.get(2 + k).copied().unwrap_or(0);
        let end = |r: &mut [u8], term: u8| {
            if n >= 2 {
                r[n - 2] = ACK;
                r[n - 1] = term;
            }
        };
        match packet[1] {
            // Set the page of the next erase, write or read.
            0x21..=0x23 => {
                self.page = u32::from_le_bytes([arg(0), arg(1), arg(2), arg(3)]) as usize % PAGES;
                self.offset = 0;
                end(r, self.terminator);
            }
            // Specifications: flags, page size, pages per block, pages, and
            // their XOR.
            0x26 => {
                let mut spec = Vec::new();
                spec.extend_from_slice(&512u16.to_le_bytes());
                spec.extend_from_slice(&(BLOCK_PAGES as u16).to_le_bytes());
                spec.extend_from_slice(&(PAGES as u32).to_le_bytes());
                let xor = spec.iter().fold(0, |a, b| a ^ b);
                r[1] = ACK;
                for (k, &b) in spec.iter().chain(std::iter::once(&xor)).enumerate() {
                    if 2 + k < n {
                        r[2 + k] = b;
                    }
                }
                r[n - 1] = self.terminator;
            }
            // Set the terminator (the new one answers, which MCMAN checks).
            0x27 => {
                self.terminator = arg(0);
                end(r, self.terminator);
            }
            0x28 => {
                r[1] = ACK;
                if n >= 4 {
                    r[2] = self.terminator;
                    r[3] = RESET_TERMINATOR;
                }
            }
            // Write up to 128 bytes at the page and offset (data from the
            // third byte after the address).
            0x42 => {
                let size = usize::from(arg(0)).min(128);
                let at = self.page * PAGE + self.offset;
                for k in 0..size.min(PAGE - self.offset) {
                    self.data[at + k] = arg(1 + k);
                }
                self.offset = (self.offset + size).min(PAGE);
                self.written = true;
                r[1] = ACK;
                r[2] = self.terminator;
                r[n - 1] = self.terminator;
            }
            // Read up to 128 bytes, then their XOR.
            0x43 => {
                let size = usize::from(arg(0)).min(128);
                let at = self.page * PAGE + self.offset;
                let bytes: Vec<u8> = (0..size)
                    .map(|k| {
                        if self.offset + k < PAGE {
                            self.data[at + k]
                        } else {
                            0xFF
                        }
                    })
                    .collect();
                self.offset = (self.offset + size).min(PAGE);
                r[1] = ACK;
                r[2] = self.terminator;
                for (k, &b) in bytes.iter().enumerate() {
                    if 3 + k < n {
                        r[3 + k] = b;
                    }
                }
                if 3 + size < n {
                    r[3 + size] = bytes.iter().fold(0, |a, b| a ^ b);
                }
                r[n - 1] = self.terminator;
            }
            // Erase the block holding the page.
            0x82 => {
                let first = self.page - self.page % BLOCK_PAGES;
                self.data[first * PAGE..(first + BLOCK_PAGES) * PAGE].fill(0xFF);
                self.written = true;
                end(r, self.terminator);
            }
            // MagicGate: the card's vectors and responses (subcommands 01,
            // 02, 04, 0F, 11, 13: 8 bytes, here zeros, then their XOR) after
            // the subcommand and the acknowledgement; the others are
            // acknowledged at their end.
            0xF0 if matches!(arg(0), 0x01 | 0x02 | 0x04 | 0x0F | 0x11 | 0x13) && n >= 13 => {
                r[2] = ACK;
                r[3..11].fill(0);
                r[11] = 0;
                r[n - 1] = self.terminator;
            }
            // Probe, end of a read or write, and the rest (MagicGate's reset
            // and key change among them): acknowledged.
            _ => end(r, self.terminator),
        }
        reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(cmd: u8, args: &[u8], len: usize) -> Vec<u8> {
        let mut p = vec![0x81, cmd];
        p.extend_from_slice(args);
        p.resize(len, 0);
        p
    }

    #[test]
    fn probe_spec_and_terminator() {
        let mut c = MemoryCard::new(None);
        assert_eq!(c.exchange(&packet(0x11, &[], 4)), [0xFF, 0xFF, ACK, 0x55]);
        let spec = c.exchange(&packet(0x26, &[], 13));
        assert_eq!(
            spec[2..],
            [ACK, 0x00, 0x02, 0x10, 0x00, 0x00, 0x40, 0x00, 0x00, 0x52, 0x55]
        );
        assert_eq!(c.exchange(&packet(0x27, &[0x5A], 5))[3..], [ACK, 0x5A]);
        assert_eq!(c.exchange(&packet(0x11, &[], 4))[3], 0x5A);
    }

    #[test]
    fn write_read_and_erase() {
        let mut c = MemoryCard::new(None);
        // Page 17: write 128 bytes, read them back with their XOR.
        c.exchange(&packet(0x22, &[17, 0, 0, 0], 9));
        let data: Vec<u8> = (0..128).map(|k| k as u8).collect();
        let mut args = vec![128];
        args.extend(&data);
        c.exchange(&packet(0x42, &args, 134));
        assert!(c.take_written().is_some());
        c.exchange(&packet(0x23, &[17, 0, 0, 0], 9));
        let read = c.exchange(&packet(0x43, &[128], 134));
        assert_eq!(read[2..4], [ACK, 0x55]);
        assert_eq!(read[4..132], data[..]);
        assert_eq!(read[132], data.iter().fold(0, |a, b| a ^ b));
        assert_eq!(read[133], 0x55);
        // Erasing the block (pages 16-31) erases it.
        c.exchange(&packet(0x21, &[20, 0, 0, 0], 9));
        c.exchange(&packet(0x82, &[], 4));
        assert!(c.data[17 * PAGE..18 * PAGE].iter().all(|&b| b == 0xFF));
    }
}
