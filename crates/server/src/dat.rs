//! DAT files, for each console: Redump's game and BIOS DATs (XML), its CUE
//! sheets and (PS1) SBI files, from redump.info, and libretro's
//! Redump-derived DAT, which adds serial numbers.

use std::collections::HashMap;
use std::io::Read;

use anyhow::Context;

use crate::library::Console;

/// Where a console's files come from, and the files they are cached in.
pub struct Sources {
    /// Redump's game DAT (a zip holding it; cached unzipped).
    pub games_url: &'static str,
    pub games_file: &'static str,
    pub serials_url: &'static str,
    pub serials_file: &'static str,
    /// Redump's BIOS DAT (XML).
    pub bios_url: &'static str,
    pub bios_file: &'static str,
    /// Redump's CUE sheets (a zip, kept as it is).
    pub cues_url: &'static str,
    pub cues_file: &'static str,
    /// Redump's SBI files (a zip, kept as it is): PS1 discs only.
    pub sbi: Option<(&'static str, &'static str)>,
}

pub fn sources(console: Console) -> &'static Sources {
    match console {
        Console::Ps1 => &Sources {
            games_url: "https://redump.info/datfile/PSX",
            games_file: "redump-psx.dat",
            serials_url: "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sony%20-%20PlayStation.dat",
            serials_file: "libretro-psx.dat",
            bios_url: "https://redump.info/bios/PSX",
            bios_file: "redump-psx-bios.dat",
            cues_url: "https://redump.info/cues/PSX",
            cues_file: "redump-psx-cues.zip",
            sbi: Some(("https://redump.info/sbi/PSX", "redump-psx-sbi.zip")),
        },
        Console::Ps2 => &Sources {
            games_url: "https://redump.info/datfile/PS2",
            games_file: "redump-ps2.dat",
            serials_url: "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sony%20-%20PlayStation%202.dat",
            serials_file: "libretro-ps2.dat",
            bios_url: "https://redump.info/bios/PS2",
            bios_file: "redump-ps2-bios.dat",
            cues_url: "https://redump.info/cues/PS2",
            cues_file: "redump-ps2-cues.zip",
            sbi: None,
        },
    }
}

/// The DATs of every console.
#[derive(Default)]
pub struct AllDats {
    pub ps1: Dats,
    pub ps2: Dats,
}

impl AllDats {
    pub fn get(&self, console: Console) -> &Dats {
        match console {
            Console::Ps1 => &self.ps1,
            Console::Ps2 => &self.ps2,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DatGame {
    pub name: String,
    pub description: Option<String>,
    pub serial: Option<String>,
    pub roms: Vec<DatRom>,
}

#[derive(Clone, Debug, Default)]
pub struct DatRom {
    pub name: String,
    pub size: u64,
    pub sha1: String,
}

impl DatGame {
    /// The track images, without the CUE sheet.
    pub fn bins(&self) -> impl Iterator<Item = &DatRom> {
        self.roms
            .iter()
            .filter(|r| !r.name.to_ascii_lowercase().ends_with(".cue"))
    }
}

/// Every DAT, indexed.
#[derive(Default)]
pub struct Dats {
    pub games: Vec<DatGame>,
    by_name: HashMap<String, usize>,
    by_sizes: HashMap<Vec<u64>, Vec<usize>>,
    by_sha1s: HashMap<Vec<String>, usize>,
    /// Serial → Redump names (libretro).
    serials: HashMap<String, Vec<SerialEntry>>,
    /// BIOS SHA-1 → description, e.g. "SCPH-1002/DTL-H1002 (Version 2.0 05/10/95 E)".
    pub bios: HashMap<String, String>,
    /// Redump's SBI files: Redump name → content (a few hundred bytes each).
    pub sbi: HashMap<String, std::sync::Arc<Vec<u8>>>,
}

struct SerialEntry {
    name: String,
    /// Size of the first track, to tell revisions and variants apart.
    first_size: Option<u64>,
}

impl Dats {
    pub fn new(redump_games: Vec<DatGame>, libretro: Vec<DatGame>, bios: Vec<DatGame>) -> Self {
        let mut d = Self {
            games: redump_games,
            ..Self::default()
        };
        for (i, g) in d.games.iter().enumerate() {
            d.by_name.insert(g.name.clone(), i);
            let mut sizes: Vec<u64> = g.bins().map(|r| r.size).collect();
            sizes.sort_unstable();
            d.by_sizes.entry(sizes).or_default().push(i);
            let mut sha1s: Vec<String> = g.bins().map(|r| r.sha1.to_ascii_lowercase()).collect();
            sha1s.sort_unstable();
            d.by_sha1s.insert(sha1s, i);
        }
        for g in libretro {
            if let Some(serial) = &g.serial {
                for s in serial.split([',', '/']).map(str::trim).filter(|s| !s.is_empty()) {
                    d.serials.entry(s.to_ascii_uppercase()).or_default().push(SerialEntry {
                        name: g.name.clone(),
                        first_size: g.roms.first().map(|r| r.size),
                    });
                }
            }
        }
        for g in bios {
            if let Some(rom) = g.roms.first() {
                d.bios.insert(
                    rom.sha1.to_ascii_lowercase(),
                    g.description.clone().unwrap_or(g.name.clone()),
                );
            }
        }
        d
    }

    pub fn game(&self, name: &str) -> Option<&DatGame> {
        self.by_name.get(name).map(|&i| &self.games[i])
    }

    /// The Redump names libretro gives a serial (several: revisions,
    /// variants).
    pub fn serial_names(&self, serial: &str) -> impl Iterator<Item = &str> {
        self.serials
            .get(&serial.to_ascii_uppercase())
            .into_iter()
            .flatten()
            .map(|e| e.name.as_str())
    }

    /// Identifies a disc from its serial and file sizes. Returns a Redump
    /// name.
    pub fn identify(&self, serial: Option<&str>, sizes: &[u64]) -> Option<String> {
        let mut sorted = sizes.to_vec();
        sorted.sort_unstable();
        let size_matches = self.by_sizes.get(&sorted);
        if let Some(entries) = serial.and_then(|s| self.serials.get(&s.to_ascii_uppercase())) {
            // Prefer the variant whose tracks all match, then the one whose
            // first track matches, then the first plain one.
            if let Some(m) = size_matches
                && let Some(e) = entries.iter().find(|e| m.iter().any(|&i| self.games[i].name == e.name))
            {
                return Some(e.name.clone());
            }
            let first = sizes.first().copied();
            if let Some(e) = entries.iter().find(|e| e.first_size.is_some() && e.first_size == first) {
                return Some(e.name.clone());
            }
            return entries.first().map(|e| e.name.clone());
        }
        // No serial: only trust a unique size match.
        match size_matches.map(Vec::as_slice) {
            Some([i]) => Some(self.games[*i].name.clone()),
            _ => None,
        }
    }

    /// Finds the Redump entry with exactly these track hashes.
    pub fn by_hashes(&self, sha1s: &[String]) -> Option<&DatGame> {
        let mut sorted: Vec<String> = sha1s.iter().map(|s| s.to_ascii_lowercase()).collect();
        sorted.sort_unstable();
        self.by_sha1s.get(&sorted).map(|&i| &self.games[i])
    }
}

/// Extracts the single `.dat` of a Redump zip.
pub fn unzip_dat(bytes: &[u8]) -> anyhow::Result<String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("reading the zip")?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        if file.name()?.ends_with(".dat") {
            let mut out = String::new();
            file.read_to_string(&mut out)?;
            return Ok(out);
        }
    }
    anyhow::bail!("no .dat in the zip")
}

/// The SBI files of Redump's zip: the name without `.sbi` → content.
pub fn unzip_sbis(bytes: &[u8]) -> anyhow::Result<HashMap<String, std::sync::Arc<Vec<u8>>>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("reading the zip")?;
    let mut out = HashMap::new();
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let full = file.name()?.into_owned();
        let name = full.rsplit('/').next().unwrap_or(&full);
        if let Some((stem, ext)) = name.rsplit_once('.')
            && ext.eq_ignore_ascii_case("sbi")
        {
            let stem = stem.to_owned();
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            out.insert(stem, std::sync::Arc::new(data));
        }
    }
    Ok(out)
}

/// Parses a Logiqx XML DAT (Redump's game and BIOS DATs: `game` or
/// `machine` entries).
pub fn parse_xml(xml: &str) -> anyhow::Result<Vec<DatGame>> {
    use quick_xml::XmlVersion;
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut games = Vec::new();
    let mut current: Option<DatGame> = None;
    let mut in_description = false;
    loop {
        match reader.read_event().context("parsing the XML DAT")? {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "game" || e.name().as_ref() == "machine" => {
                let mut g = DatGame::default();
                for a in e.attributes().flatten() {
                    if a.key.as_ref() == "name" {
                        g.name = a.normalized_value(XmlVersion::Implicit1_0)?.into_owned();
                    }
                }
                current = Some(g);
            }
            Event::Start(e) if e.name().as_ref() == "description" => in_description = true,
            Event::End(e) if e.name().as_ref() == "description" => in_description = false,
            Event::Text(t) if in_description => {
                if let Some(g) = current.as_mut() {
                    g.description = Some(t.xml_content(XmlVersion::Implicit1_0).into_owned());
                }
            }
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "rom" => {
                let mut rom = DatRom::default();
                for a in e.attributes().flatten() {
                    let v = a.normalized_value(XmlVersion::Implicit1_0)?;
                    match a.key.as_ref() {
                        "name" => rom.name = v.into_owned(),
                        "size" => rom.size = v.parse().unwrap_or(0),
                        "sha1" => rom.sha1 = v.to_ascii_lowercase(),
                        _ => {}
                    }
                }
                if let Some(g) = current.as_mut() {
                    g.roms.push(rom);
                }
            }
            Event::End(e) if e.name().as_ref() == "game" || e.name().as_ref() == "machine" => {
                if let Some(g) = current.take() {
                    games.push(g);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(games)
}

/// Parses a clrmamepro DAT (libretro's, Redump's BIOS DAT).
pub fn parse_clrmamepro(text: &str) -> Vec<DatGame> {
    let tokens = tokenize(text);
    let mut games = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == Token::Word("game".into()) && tokens.get(i + 1) == Some(&Token::Open) {
            let (block, next) = parse_block(&tokens, i + 2);
            i = next;
            let mut g = DatGame::default();
            for (k, v) in block {
                match (k.as_str(), v) {
                    ("name", Value::Text(s)) => g.name = s,
                    ("description", Value::Text(s)) => g.description = Some(s),
                    ("serial", Value::Text(s)) => g.serial = Some(s),
                    ("rom", Value::Block(fields)) => {
                        let mut rom = DatRom::default();
                        for (k, v) in fields {
                            if let Value::Text(s) = v {
                                match k.as_str() {
                                    "name" => rom.name = s,
                                    "size" => rom.size = s.parse().unwrap_or(0),
                                    "sha1" => rom.sha1 = s.to_ascii_lowercase(),
                                    _ => {}
                                }
                            }
                        }
                        g.roms.push(rom);
                    }
                    _ => {}
                }
            }
            games.push(g);
        } else {
            i += 1;
        }
    }
    games
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Word(String),
}

enum Value {
    Text(String),
    Block(Vec<(String, Value)>),
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '(' => {
                chars.next();
                out.push(Token::Open);
            }
            ')' => {
                chars.next();
                out.push(Token::Close);
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    s.push(c);
                }
                out.push(Token::Word(s));
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => {
                // Parentheses inside a word are part of it: the ROM names
                // `ps-22j(v).bin` and `ps-41a(w).bin` are unquoted. Blocks'
                // own parentheses stand apart, after whitespace.
                let mut s = String::new();
                let mut depth = 0usize;
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || (c == ')' && depth == 0) {
                        break;
                    }
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    s.push(c);
                    chars.next();
                }
                out.push(Token::Word(s));
            }
        }
    }
    out
}

/// Parses `key value` pairs until the closing parenthesis; returns them and
/// the index after it.
fn parse_block(tokens: &[Token], mut i: usize) -> (Vec<(String, Value)>, usize) {
    let mut out = Vec::new();
    while i < tokens.len() {
        match &tokens[i] {
            Token::Close => return (out, i + 1),
            Token::Word(key) => match tokens.get(i + 1) {
                Some(Token::Open) => {
                    let (block, next) = parse_block(tokens, i + 2);
                    out.push((key.clone(), Value::Block(block)));
                    i = next;
                }
                Some(Token::Word(v)) => {
                    out.push((key.clone(), Value::Text(v.clone())));
                    i += 2;
                }
                _ => i += 1,
            },
            Token::Open => {
                let (_, next) = parse_block(tokens, i + 1);
                i = next;
            }
        }
    }
    (out, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parentheses_in_unquoted_names() {
        // Redump's PS1 BIOS DAT names two ROMs so.
        let games = parse_clrmamepro(
            r#"game (
	name "ps-41a(w)"
	description "SCPH-7000W (Version 4.1 11/14/97 A)"
	rom ( name ps-41a(w).bin size 524288 crc b7c43dad md5 b84be139db3ee6cbd075630aa20a6553 sha1 1b0dbdb23da9dc0776aac58d0755dc80fea20975 )
)
game (
	name "ps-41e"
	rom ( name ps-41e.bin size 524288 sha1 8d5de56a79954f29e9006929ba3fed9b6a418c1d )
)"#,
        );
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].roms[0].name, "ps-41a(w).bin");
        assert_eq!(games[0].roms[0].sha1, "1b0dbdb23da9dc0776aac58d0755dc80fea20975");
        assert_eq!(games[1].roms[0].sha1, "8d5de56a79954f29e9006929ba3fed9b6a418c1d");
    }

    #[test]
    fn clrmamepro_and_identification() {
        let libretro = parse_clrmamepro(
            r#"clrmamepro ( name "x" )
game (
	name "Rayman (Europe) (En,Fr,De)"
	serial "SLES-00049"
	rom ( name "Rayman (Europe) (En,Fr,De) (Track 01).bin" size 100 crc 0 sha1 AB serial "SLES-00049" )
)
game (
	name "Rayman (Europe) (En,Fr,De) (EDC)"
	serial "SLES-00049"
	rom ( name "x.bin" size 100 sha1 CD )
)"#,
        );
        assert_eq!(libretro.len(), 2);
        assert_eq!(libretro[0].roms[0].size, 100);
        let redump = parse_xml(
            r#"<?xml version="1.0"?><datafile>
<game name="Rayman (Europe) (En,Fr,De)"><description>Rayman (Europe) (En,Fr,De)</description>
<rom name="a.cue" size="5" sha1="00"/><rom name="a (Track 01).bin" size="100" sha1="AA"/><rom name="a (Track 02).bin" size="7" sha1="BB"/></game>
</datafile>"#,
        )
        .unwrap();
        let bios = parse_clrmamepro(
            r#"game ( name "ps-20e" description "SCPH-1002/DTL-H1002 (Version 2.0 05/10/95 E)" rom ( name ps-20e.bin size 524288 sha1 20b98f3d80f11cbf5a7bfd0779b0e63760ecc62c ) )"#,
        );
        let dats = Dats::new(redump, libretro, bios);
        assert_eq!(
            dats.identify(Some("SLES-00049"), &[100, 7]).as_deref(),
            Some("Rayman (Europe) (En,Fr,De)")
        );
        assert_eq!(
            dats.identify(None, &[7, 100]).as_deref(),
            Some("Rayman (Europe) (En,Fr,De)")
        );
        assert!(dats.identify(None, &[1]).is_none());
        assert!(dats.by_hashes(&["bb".into(), "aa".into()]).is_some());
        assert!(dats.bios["20b98f3d80f11cbf5a7bfd0779b0e63760ecc62c"].starts_with("SCPH-1002"));
    }

    #[test]
    fn redump_bios_xml_and_sbi_zip() {
        // redump.info's BIOS DATs: Logiqx XML, `machine` entries.
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<datafile>
	<header><name>Sony - PlayStation - BIOS Images</name></header>
	<machine name="ps-10j">
		<description>SCPH-1000/DTL-H1000 (Version 1.0 J)</description>
		<rom name="ps-10j.bin" size="524288" crc="3b601fc8" sha1="343883A7B555646DA8CEE54AADD2795B6E7DD070" />
	</machine>
</datafile>"#;
        let dats = Dats::new(Vec::new(), Vec::new(), parse_xml(xml).unwrap());
        assert_eq!(
            dats.bios["343883a7b555646da8cee54aadd2795b6e7dd070"],
            "SCPH-1000/DTL-H1000 (Version 1.0 J)"
        );
        // Its SBI files: one zip, named after the discs.
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in [("Ape Escape (France).sbi", &b"one"[..]), ("readme.txt", &b"no"[..])] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zip, data).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let sbi = unzip_sbis(&bytes).unwrap();
        assert_eq!(sbi.len(), 1);
        assert_eq!(sbi["Ape Escape (France)"].as_slice(), b"one");
    }
}
