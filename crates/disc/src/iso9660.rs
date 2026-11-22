//! Just enough ISO 9660 to read SYSTEM.CNF and identify a disc.

use crate::{DATA_SECTOR_SIZE, Disc, FileReader, ReadError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IsoError {
    Read(ReadError),
    NotIso9660,
    NotFound(String),
}

impl std::fmt::Display for IsoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => e.fmt(f),
            Self::NotIso9660 => f.write_str("no ISO 9660 primary volume descriptor"),
            Self::NotFound(p) => write!(f, "{p} not found"),
        }
    }
}

impl std::error::Error for IsoError {}

impl From<ReadError> for IsoError {
    fn from(e: ReadError) -> Self {
        Self::Read(e)
    }
}

/// What SYSTEM.CNF tells about a disc.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemCnf {
    /// The ISO 9660 volume identifier.
    pub volume_id: String,
    /// The boot executable, e.g. `cdrom:\SLUS_012.34;1`.
    pub boot: String,
    /// The serial derived from the boot executable, e.g. `SLUS-01234`.
    pub serial: Option<String>,
    /// A PlayStation 2 disc: SYSTEM.CNF names its executable with `BOOT2`
    /// (`cdrom0:\SLUS_203.12;1`) rather than `BOOT`.
    pub ps2: bool,
}

/// Finds a file by path (`\`- or `/`-separated, case-insensitive, version
/// suffix optional). Returns its first sector and size in bytes.
pub fn find_file<R: FileReader>(disc: &mut Disc<R>, path: &str) -> Result<(u32, u32), IsoError> {
    let mut sector = [0u8; DATA_SECTOR_SIZE];
    disc.read_data(16, &mut sector)?;
    if &sector[1..6] != b"CD001" || sector[0] != 1 {
        return Err(IsoError::NotIso9660);
    }
    let (mut lba, mut size) = record_extent(&sector[156..]);
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        let want_dir = i + 1 < parts.len();
        let found = find_in_directory(disc, lba, size, part, want_dir)?;
        (lba, size) = found.ok_or_else(|| IsoError::NotFound(path.to_owned()))?;
    }
    Ok((lba, size))
}

fn record_extent(record: &[u8]) -> (u32, u32) {
    let lba = u32::from_le_bytes(record[2..6].try_into().expect("4 bytes"));
    let size = u32::from_le_bytes(record[10..14].try_into().expect("4 bytes"));
    (lba, size)
}

fn find_in_directory<R: FileReader>(
    disc: &mut Disc<R>,
    lba: u32,
    size: u32,
    name: &str,
    want_dir: bool,
) -> Result<Option<(u32, u32)>, IsoError> {
    let name = name.split(';').next().unwrap_or(name);
    let mut sector = [0u8; DATA_SECTOR_SIZE];
    // Bounded, so that a corrupt directory cannot make us read forever.
    let sectors = size.div_ceil(DATA_SECTOR_SIZE as u32).min(64);
    for s in 0..sectors {
        disc.read_data(lba + s, &mut sector)?;
        let mut pos = 0;
        while pos + 33 < DATA_SECTOR_SIZE {
            let len = usize::from(sector[pos]);
            if len == 0 {
                // Records do not cross sectors: the rest is padding.
                break;
            }
            let Some(record) = sector.get(pos..pos + len) else {
                break;
            };
            let name_len = usize::from(record[32]);
            let is_dir = record[25] & 2 != 0;
            if let Some(raw) = record.get(33..33 + name_len) {
                let entry = String::from_utf8_lossy(raw);
                let entry = entry.split(';').next().unwrap_or_default();
                let entry = entry.strip_suffix('.').unwrap_or(entry);
                if is_dir == want_dir && entry.eq_ignore_ascii_case(name) {
                    return Ok(Some(record_extent(record)));
                }
            }
            pos += len;
        }
    }
    Ok(None)
}

/// Reads a whole (small) file.
fn read_file<R: FileReader>(disc: &mut Disc<R>, lba: u32, size: u32, limit: u32) -> Result<Vec<u8>, IsoError> {
    let size = size.min(limit) as usize;
    let mut out = Vec::with_capacity(size);
    let mut sector = [0u8; DATA_SECTOR_SIZE];
    let mut lba = lba;
    while out.len() < size {
        disc.read_data(lba, &mut sector)?;
        let n = (size - out.len()).min(DATA_SECTOR_SIZE);
        out.extend_from_slice(&sector[..n]);
        lba += 1;
    }
    Ok(out)
}

/// Reads SYSTEM.CNF and derives the serial. Discs without SYSTEM.CNF boot
/// PSX.EXE and have no serial.
pub fn read_system_cnf<R: FileReader>(disc: &mut Disc<R>) -> Result<SystemCnf, IsoError> {
    let mut pvd = [0u8; DATA_SECTOR_SIZE];
    disc.read_data(16, &mut pvd)?;
    let volume_id = String::from_utf8_lossy(&pvd[40..72]).trim().to_owned();
    let (boot, ps2) = match find_file(disc, "SYSTEM.CNF") {
        Ok((lba, size)) => {
            let text = read_file(disc, lba, size, 4096)?;
            let text = String::from_utf8_lossy(&text);
            match boot_line(&text, "BOOT2") {
                Some(boot) => (boot, true),
                None => (boot_line(&text, "BOOT").unwrap_or_default(), false),
            }
        }
        Err(IsoError::NotFound(_)) => {
            find_file(disc, "PSX.EXE")?;
            ("cdrom:\\PSX.EXE;1".to_owned(), false)
        }
        Err(e) => return Err(e),
    };
    let serial = serial_from_boot_path(&boot);
    Ok(SystemCnf {
        volume_id,
        boot,
        serial,
        ps2,
    })
}

/// The value of `key` in SYSTEM.CNF (`BOOT = cdrom:\SLUS_012.34;1`).
fn boot_line(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (k, value) = line.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(key)
            .then(|| value.split_whitespace().next().unwrap_or_default().to_owned())
    })
}

/// `cdrom:\SLUS_012.34;1` → `SLUS-01234`. Returns `None` for executables
/// that are not named after a serial.
pub fn serial_from_boot_path(boot: &str) -> Option<String> {
    let file = boot.rsplit(['\\', '/', ':']).next()?;
    let file = file.split(';').next()?.to_ascii_uppercase();
    let (prefix, number) = file.split_once(['_', '-'])?;
    let digits: String = number.chars().filter(|c| *c != '.').collect();
    let valid = prefix.len() == 4
        && prefix.chars().all(|c| c.is_ascii_alphabetic())
        && digits.len() == 5
        && digits.chars().all(|c| c.is_ascii_digit());
    valid.then(|| format!("{prefix}-{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Layout;
    use crate::tests::MemReader;

    fn record(name: &[u8], lba: u32, size: u32, dir: bool) -> Vec<u8> {
        let len = 33 + name.len() + (1 - name.len() % 2);
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[6..10].copy_from_slice(&lba.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[25] = if dir { 2 } else { 0 };
        r[32] = name.len() as u8;
        r[33..33 + name.len()].copy_from_slice(name);
        r
    }

    #[test]
    fn reads_system_cnf() {
        let mut iso = vec![0u8; 2048 * 24];
        let pvd = 16 * 2048;
        iso[pvd] = 1;
        iso[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        iso[pvd + 40..pvd + 72].copy_from_slice(&[b' '; 32]);
        iso[pvd + 40..pvd + 44].copy_from_slice(b"GAME");
        let root = record(b"\0", 20, 2048, true);
        iso[pvd + 156..pvd + 156 + root.len()].copy_from_slice(&root);
        let entry = record(b"SYSTEM.CNF;1", 21, 60, false);
        iso[20 * 2048..20 * 2048 + entry.len()].copy_from_slice(&entry);
        let cnf = b"BOOT = cdrom:\\SLUS_012.34;1\r\nTCB = 4\r\nEVENT = 10\r\nSTACK = 801FFFF0\r\n";
        iso[21 * 2048..21 * 2048 + cnf.len()].copy_from_slice(cnf);

        let layout = Layout::single_file("g.iso", iso.len() as u64).unwrap();
        let mut disc = Disc::new(layout, MemReader(vec![iso]));
        let cnf = read_system_cnf(&mut disc).unwrap();
        assert_eq!(cnf.volume_id, "GAME");
        assert_eq!(cnf.boot, "cdrom:\\SLUS_012.34;1");
        assert_eq!(cnf.serial.as_deref(), Some("SLUS-01234"));
        assert!(!cnf.ps2);
    }

    #[test]
    fn boot_lines() {
        let ps2 = "BOOT2 = cdrom0:\\SLES_519.99;1\r\nVER = 1.00\r\nVMODE = PAL\r\n";
        assert_eq!(boot_line(ps2, "BOOT2").as_deref(), Some("cdrom0:\\SLES_519.99;1"));
        assert_eq!(boot_line(ps2, "BOOT"), None);
        assert_eq!(
            serial_from_boot_path("cdrom0:\\SLES_519.99;1").as_deref(),
            Some("SLES-51999")
        );
    }

    #[test]
    fn serials() {
        assert_eq!(
            serial_from_boot_path("cdrom:SCUS_944.26;1").as_deref(),
            Some("SCUS-94426")
        );
        assert_eq!(
            serial_from_boot_path("cdrom:\\EXE\\slps_000.01;1").as_deref(),
            Some("SLPS-00001")
        );
        assert_eq!(serial_from_boot_path("cdrom:\\MAIN.EXE;1"), None);
    }
}
