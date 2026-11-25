//! BIOS image identification, from the ROM's file directory.
//!
//! A PS2 BIOS starts with a small file system: a directory of 16-byte
//! entries (name, extended-information size, size), found by its first
//! entry, `RESET`, and files laid out one after another from the start of
//! the ROM, each aligned to 16 bytes. The `ROMVER` file describes the ROM:
//! `0200EC20040614` is version 2.00, Europe, a console ROM (C), built on
//! 2004-06-14.
//!
//! Other PS2 firmware, kept in a library with the BIOSes but booting
//! nothing here (`identify_firmware`): the DVD player (`rom1`, the same file
//! system, with `DVDVER`), the BIOS of PS2-based arcade boards (a `ROMVER`
//! of type Z, in 2 MiB), and the MechaCon's (the disc drive's controller in
//! slim consoles: no file system, known by its size and name).

/// Size of a BIOS dump: 4 MiB for every model.
pub const BIOS_SIZE: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiosInfo {
    /// e.g. "2.00".
    pub version: String,
    /// e.g. "2004-06-14".
    pub date: String,
    pub region: Region,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    NorthAmerica,
    Europe,
    Japan,
    Asia,
    China,
    Unknown,
}

impl Region {
    fn from_letter(letter: u8) -> Self {
        match letter {
            b'A' => Self::NorthAmerica,
            b'E' => Self::Europe,
            b'J' => Self::Japan,
            b'H' => Self::Asia,
            b'C' => Self::China,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NorthAmerica => "NTSC-U",
            Self::Europe => "PAL",
            Self::Japan => "NTSC-J",
            Self::Asia => "NTSC-Asia",
            Self::China => "NTSC-China",
            Self::Unknown => "unknown",
        }
    }
}

/// The directory of the ROM's file system: (name, offset, size) of each
/// file.
fn romdir(rom: &[u8]) -> Option<Vec<(&[u8], usize, usize)>> {
    const ENTRY: usize = 16;
    // The directory is in the first file, RESET, within its first 64 KiB.
    let start = (0..0x10000.min(rom.len()))
        .step_by(ENTRY)
        .find(|&at| rom.get(at..at + 6) == Some(b"RESET\0"))?;
    let mut files = Vec::new();
    let mut offset = 0usize;
    for entry in rom[start..].as_chunks::<ENTRY>().0.iter().take(256) {
        let name = &entry[..10];
        let len = name.iter().position(|&b| b == 0).unwrap_or(10);
        if len == 0 {
            return Some(files);
        }
        let size = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        files.push((&name[..len], offset, size));
        offset = offset.checked_add(size.div_ceil(16) * 16)?;
    }
    None
}

/// Identifies a BIOS dump from its size and its `ROMVER` file. Returns
/// `None` for anything that does not look like a PlayStation 2 BIOS.
pub fn identify(rom: &[u8]) -> Option<BiosInfo> {
    if rom.len() != BIOS_SIZE {
        return None;
    }
    let (_, offset, size) = romdir(rom)?.into_iter().find(|(name, ..)| *name == b"ROMVER")?;
    let text = rom.get(offset..offset + size.min(16))?;
    let text = std::str::from_utf8(text).ok()?.trim_end_matches(['\n', '\0']);
    // VVVV R T YYYYMMDD
    if text.len() < 14 || !text.is_char_boundary(4) || !text.is_ascii() {
        return None;
    }
    let (major, minor) = (&text[0..2], &text[2..4]);
    let date = &text[6..14];
    if !(major.bytes().chain(minor.bytes()).chain(date.bytes())).all(|b| b.is_ascii_digit()) {
        return None;
    }
    // An arcade board's BIOS: not a console's (`identify_firmware`).
    if text.as_bytes()[5] == b'Z' {
        return None;
    }
    Some(BiosInfo {
        version: format!("{}.{minor}", major.trim_start_matches('0').max("0")),
        date: format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8]),
        region: Region::from_letter(text.as_bytes()[4]),
    })
}

/// The size of a MechaCon firmware dump (256 KiB of flash, 16 KiB more).
pub const MECHACON_SIZE: usize = 272 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareKind {
    /// The console's DVD video player (`rom1`).
    DvdPlayer,
    /// The BIOS of a PS2-based arcade board.
    Arcade,
    /// The disc drive's controller (slim consoles).
    MechaCon,
}

impl FirmwareKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DvdPlayer => "dvd",
            Self::Arcade => "arcade",
            Self::MechaCon => "mechacon",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareInfo {
    pub kind: FirmwareKind,
    /// e.g. "1.10U" (a DVD player's, with its region letter), "1.00",
    /// "5.00".
    pub version: String,
    /// e.g. "2000-07-26", when known.
    pub date: Option<String>,
}

/// Identifies PS2 firmware other than a console's BIOS, from its content
/// (and, for the MechaCon's, which has no file system, its size and file
/// name). `None` for anything else.
pub fn identify_firmware(rom: &[u8], name: &str) -> Option<FirmwareInfo> {
    if rom.len() == MECHACON_SIZE {
        return mechacon(name);
    }
    if rom.len() != BIOS_SIZE && rom.len() != BIOS_SIZE / 2 {
        return None;
    }
    let files = romdir(rom)?;
    let file = |want: &[u8]| {
        files
            .iter()
            .find(|(n, ..)| *n == want)
            .and_then(|&(_, offset, size)| rom.get(offset..offset + size))
    };
    let text = |b: &[u8]| {
        String::from_utf8_lossy(b)
            .trim_end_matches(['\n', '\0'])
            .trim()
            .to_owned()
    };
    // The DVD player: DVDVER, or one per region (DVDVERJ, DVDVERU...: the
    // version is theirs, without region letters).
    let versions: Vec<String> = files
        .iter()
        .filter(|(n, ..)| n.starts_with(b"DVDVER"))
        .filter_map(|&(_, offset, size)| rom.get(offset..offset + size).map(text))
        .collect();
    // The image's build date: EXTINFO's first "20000726-221712".
    let built = file(b"EXTINFO").and_then(|e| {
        e.windows(9).find_map(|w| {
            (w[..8].iter().all(u8::is_ascii_digit) && w[8] == b'-').then(|| {
                let d = std::str::from_utf8(&w[..8]).unwrap_or_default();
                format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8])
            })
        })
    });
    if !versions.is_empty() {
        // DVDID names the version (3.04M's DVDVER is a build tag); else
        // DVDVER, else the regions' own, without their letters.
        let version = match file(b"DVDID").or_else(|| file(b"DVDVER")) {
            Some(v) => text(v),
            None => versions[0]
                .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                .to_owned(),
        };
        return Some(FirmwareInfo {
            kind: FirmwareKind::DvdPlayer,
            version,
            date: built,
        });
    }
    // An arcade board's BIOS: a ROMVER of type Z (the same in every build:
    // the build's date is EXTINFO's).
    let romver = text(file(b"ROMVER")?);
    let b = romver.as_bytes();
    if b.len() >= 14 && b[5] == b'Z' && b[..4].iter().chain(&b[6..14]).all(u8::is_ascii_digit) {
        return Some(FirmwareInfo {
            kind: FirmwareKind::Arcade,
            version: format!("{}.{}", romver[0..2].trim_start_matches('0').max("0"), &romver[2..4]),
            date: built.or_else(|| Some(format!("{}-{}-{}", &romver[6..10], &romver[10..12], &romver[12..14]))),
        });
    }
    None
}

/// A MechaCon firmware dump, by its file name: "ps2-mechacon-500-20030120..."
/// is version 5.00, of 2003-01-20.
fn mechacon(name: &str) -> Option<FirmwareInfo> {
    let lower = name.to_ascii_lowercase();
    let after = &lower[lower.find("mechacon")? + "mechacon".len()..];
    let mut fields = after.split(|c: char| !c.is_ascii_digit()).filter(|f| !f.is_empty());
    let version = fields
        .next()
        .filter(|v| v.len() == 3)
        .map(|v| format!("{}.{}", &v[..1], &v[1..]))
        .unwrap_or_else(|| "unknown".to_owned());
    let date = fields
        .next()
        .filter(|d| d.len() == 8)
        .map(|d| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]));
    Some(FirmwareInfo {
        kind: FirmwareKind::MechaCon,
        version,
        date,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROM with a directory (RESET, ROMDIR, ROMVER) and a ROMVER file.
    fn rom(romver: &[u8]) -> Vec<u8> {
        rom_with(BIOS_SIZE, &[(b"ROMVER", romver)])
    }

    /// A ROM of `size` with a directory (RESET, ROMDIR) and these files.
    fn rom_with(size: usize, files: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut rom = vec![0u8; size];
        let entry = |name: &[u8], size: u32| {
            let mut e = [0u8; 16];
            e[..name.len()].copy_from_slice(name);
            e[12..].copy_from_slice(&size.to_le_bytes());
            e
        };
        // RESET holds the directory, at 0x100: 0x200 bytes long.
        let mut dir = vec![entry(b"RESET", 0x200), entry(b"ROMDIR", 0x40)];
        dir.extend(files.iter().map(|(n, c)| entry(n, c.len() as u32)));
        for (i, e) in dir.iter().enumerate() {
            rom[0x100 + i * 16..0x110 + i * 16].copy_from_slice(e);
        }
        let mut at = 0x240;
        for (_, content) in files {
            rom[at..at + content.len()].copy_from_slice(content);
            at += content.len().div_ceil(16) * 16;
        }
        rom
    }

    #[test]
    fn identifies_other_firmware() {
        let dvd = rom_with(
            BIOS_SIZE / 2,
            &[
                (b"EXTINFO", b"\0\0\x04\x0120000726-221712,"),
                (b"DVDID", b"1.10U\n"),
                (b"DVDVER", b"1.10U\n"),
            ],
        );
        assert_eq!(
            identify_firmware(&dvd, "x.bin"),
            Some(FirmwareInfo {
                kind: FirmwareKind::DvdPlayer,
                version: "1.10U".into(),
                date: Some("2000-07-26".into()),
            })
        );
        let multi = rom_with(BIOS_SIZE, &[(b"DVDVERJ", b"3.11\n"), (b"DVDVERU", b"3.11U\n")]);
        assert_eq!(
            identify_firmware(&multi, "x.bin").map(|f| f.version),
            Some("3.11".into())
        );
        let arcade = rom_with(BIOS_SIZE / 2, &[(b"ROMVER", b"0100TZ20000306\n")]);
        let info = identify_firmware(&arcade, "x.bin").unwrap();
        assert_eq!((info.kind, info.version.as_str()), (FirmwareKind::Arcade, "1.00"));
        assert_eq!(info.date.as_deref(), Some("2000-03-06"));
        let built = rom_with(
            BIOS_SIZE / 2,
            &[
                (b"EXTINFO", b"\0\x03\x0320091029-174532,rom0"),
                (b"ROMVER", b"0100TZ20000306\n"),
            ],
        );
        assert_eq!(
            identify_firmware(&built, "x.bin").and_then(|f| f.date),
            Some("2009-10-29".into())
        );
        let tagged = rom_with(BIOS_SIZE / 2, &[(b"DVDID", b"3.04M\n"), (b"DVDVER", b"cvrM040402a\n")]);
        assert_eq!(
            identify_firmware(&tagged, "x.bin").map(|f| f.version),
            Some("3.04M".into())
        );
        // Never a console's BIOS, even of a console's size.
        assert_eq!(identify(&rom(b"0100TZ20000306")), None);
        let m = identify_firmware(&vec![0; MECHACON_SIZE], "ps2-mechacon-504-20030408-232700.bin").unwrap();
        assert_eq!((m.kind, m.version.as_str()), (FirmwareKind::MechaCon, "5.04"));
        assert_eq!(m.date.as_deref(), Some("2003-04-08"));
        assert_eq!(identify_firmware(&vec![0; MECHACON_SIZE], "other.bin"), None);
        // A console's BIOS is not this.
        assert_eq!(identify_firmware(&rom(b"0200EC20040614"), "x.bin"), None);
    }

    #[test]
    fn reads_the_rom_version() {
        assert_eq!(
            identify(&rom(b"0200EC20040614\n\0")),
            Some(BiosInfo {
                version: "2.00".into(),
                date: "2004-06-14".into(),
                region: Region::Europe,
            })
        );
        assert_eq!(
            identify(&rom(b"0006AD20261105")).map(|b| b.version),
            Some("0.06".into())
        );
    }

    #[test]
    fn rejects_other_files() {
        assert_eq!(identify(&rom(b"not a version!!")), None);
        assert_eq!(identify(&vec![0u8; BIOS_SIZE]), None);
        assert_eq!(identify(&[0u8; 512 * 1024]), None);
    }
}
