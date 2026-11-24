//! A game's own pointers to its details, for when the automatic lookup
//! fails (a title shared by several games, say): a YAML file next to its
//! image, named like it (`disc.chd` → `disc.yaml`), or named after its
//! serial (`SLES-01893.yaml`) anywhere in its console's game directories:
//!
//! ```yaml
//! # Bomberman (Europe) is Bomberman Party Edition.
//! wikipedia: Bomberman Party Edition
//! wikidata: Q2909728
//! igdb: bomberman-party-edition
//! links:
//!   Eurogamer: https://www.eurogamer.net/...
//! ```
//!
//! Every key is optional. `wikipedia` (an English article's title, or an
//! article's URL in any language) and `wikidata` (an item, or its URL)
//! replace the search; `igdb` (a game's slug or URL) is used when IGDB is
//! configured. `thumbnails` is the name libretro-thumbnails files the game's
//! images under, when the Redump name and its variants do not find them.
//! `links` are shown on the game's page: sites without an open API
//! (Eurogamer, IGN...) are not read.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    pub wikipedia: Option<String>,
    pub wikidata: Option<String>,
    pub igdb: Option<String>,
    /// The name libretro-thumbnails files the game's images under, when it
    /// is not the Redump name, nor an older or differently capitalised one.
    pub thumbnails: Option<String>,
    #[serde(default)]
    pub links: BTreeMap<String, String>,
}

impl Overrides {
    /// Parses and checks a file.
    pub fn parse(text: &str) -> Result<Self, String> {
        // An empty file (or only comments) is no overrides.
        if text
            .lines()
            .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
        {
            return Ok(Self::default());
        }
        let o: Self = serde_saphyr::from_str(text).map_err(|e| e.to_string())?;
        if let Some(w) = &o.wikipedia
            && w.contains("://")
            && o.wikipedia_page().is_none()
        {
            return Err(format!("wikipedia: {w} is not a Wikipedia article's URL"));
        }
        if let Some(w) = &o.wikidata
            && o.wikidata_item().is_none()
        {
            return Err(format!("wikidata: {w} is not a Wikidata item (Q123) or its URL"));
        }
        if let Some(i) = &o.igdb
            && o.igdb_slug().is_none()
        {
            return Err(format!("igdb: {i} is not an IGDB game's slug or URL"));
        }
        if let Some(t) = &o.thumbnails
            && o.thumbnails_name().is_none()
        {
            return Err(format!(
                "thumbnails: {t} is not an image name (a file name, without a directory)"
            ));
        }
        for (name, url) in &o.links {
            if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains(char::is_whitespace) {
                return Err(format!("links: {name}: {url} is not a web address"));
            }
        }
        Ok(o)
    }

    /// The Wikipedia article: (language, title).
    pub fn wikipedia_page(&self) -> Option<(String, String)> {
        let w = self.wikipedia.as_deref()?.trim();
        if !w.contains("://") {
            return (!w.is_empty()).then(|| ("en".to_owned(), w.to_owned()));
        }
        // https://fr.wikipedia.org/wiki/Les_Razmoket_%C3%A0_Paris
        let rest = w.strip_prefix("https://").or_else(|| w.strip_prefix("http://"))?;
        let (host, path) = rest.split_once('/')?;
        let lang = host.strip_suffix(".wikipedia.org")?.trim_start_matches("www.");
        let lang = lang.strip_suffix(".m").unwrap_or(lang);
        let title = path.strip_prefix("wiki/")?.split(['#', '?']).next()?;
        if lang.is_empty() || !lang.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') || title.is_empty() {
            return None;
        }
        Some((lang.to_owned(), percent_decode(title).replace('_', " ")))
    }

    /// The Wikidata item: "Q2909728".
    pub fn wikidata_item(&self) -> Option<String> {
        let w = self.wikidata.as_deref()?.trim();
        let id = w.rsplit('/').next()?;
        (id.len() > 1 && id.starts_with('Q') && id[1..].bytes().all(|b| b.is_ascii_digit())).then(|| id.to_owned())
    }

    /// The IGDB slug: "bomberman-party-edition".
    pub fn igdb_slug(&self) -> Option<String> {
        let i = self.igdb.as_deref()?.trim().trim_end_matches('/');
        let slug = if i.contains("://") {
            i.split_once("igdb.com/games/")?.1
        } else {
            i
        };
        (!slug.is_empty()
            && slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        .then(|| slug.to_owned())
    }

    /// The libretro-thumbnails name: "WipEout 3 - Special Edition (Europe)
    /// (En,Fr,De,Es,It)" (a trailing `.png` is dropped).
    pub fn thumbnails_name(&self) -> Option<String> {
        let t = self.thumbnails.as_deref()?.trim();
        let t = t.strip_suffix(".png").unwrap_or(t).trim_end();
        (!t.is_empty() && !t.contains(['/', '\\']) && !t.chars().any(char::is_control)).then(|| t.to_owned())
    }

    /// Whether the details come from the pages given, not from a search.
    pub fn pins_wiki(&self) -> bool {
        self.wikipedia_page().is_some() || self.wikidata_item().is_some()
    }
}

/// "%C3%A0" → "à".
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_file() {
        let o = Overrides::parse(
            "# Bomberman (Europe) is Bomberman Party Edition.\n\
             wikipedia: Bomberman Party Edition\n\
             wikidata: https://www.wikidata.org/wiki/Q2909728\n\
             igdb: https://www.igdb.com/games/bomberman-party-edition\n\
             links:\n  Eurogamer: https://www.eurogamer.net/bomberman\n  IGN: https://www.ign.com/games/bomberman\n",
        )
        .unwrap();
        assert_eq!(
            o.wikipedia_page(),
            Some(("en".into(), "Bomberman Party Edition".into()))
        );
        assert_eq!(o.wikidata_item().as_deref(), Some("Q2909728"));
        assert_eq!(o.igdb_slug().as_deref(), Some("bomberman-party-edition"));
        assert_eq!(o.links.len(), 2);
        assert!(o.pins_wiki());
    }

    #[test]
    fn reads_article_urls() {
        let o = Overrides::parse("wikipedia: https://fr.wikipedia.org/wiki/Les_Razmoket_%C3%A0_Paris#Jeu\n").unwrap();
        assert_eq!(o.wikipedia_page(), Some(("fr".into(), "Les Razmoket à Paris".into())));
        let o = Overrides::parse("wikipedia: https://en.m.wikipedia.org/wiki/Tarzan_(1999_video_game)\n").unwrap();
        assert_eq!(
            o.wikipedia_page(),
            Some(("en".into(), "Tarzan (1999 video game)".into()))
        );
    }

    #[test]
    fn thumbnails_names() {
        let o = Overrides::parse("thumbnails: WipEout 3 - Special Edition (Europe) (En,Fr,De,Es,It)\n").unwrap();
        assert_eq!(
            o.thumbnails_name().as_deref(),
            Some("WipEout 3 - Special Edition (Europe) (En,Fr,De,Es,It)")
        );
        let o = Overrides::parse("thumbnails: \"Rayman (USA).png\"\n").unwrap();
        assert_eq!(o.thumbnails_name().as_deref(), Some("Rayman (USA)"));
        assert!(!o.pins_wiki(), "images only: the details are still searched");
        assert!(Overrides::parse("thumbnails: Named_Boxarts/Rayman (USA)\n").is_err());
    }

    #[test]
    fn empty_files_are_fine() {
        assert_eq!(Overrides::parse("").unwrap(), Overrides::default());
        assert_eq!(Overrides::parse("# nothing yet\n").unwrap(), Overrides::default());
        assert!(!Overrides::default().pins_wiki());
    }

    #[test]
    fn rejects_mistakes() {
        assert!(Overrides::parse("wikipedai: Typo\n").is_err(), "unknown key");
        assert!(Overrides::parse("wikipedia: https://example.com/wiki/X\n").is_err());
        assert!(Overrides::parse("wikidata: Bomberman\n").is_err());
        assert!(Overrides::parse("igdb: Bomberman Party Edition\n").is_err());
        assert!(Overrides::parse("links:\n  IGN: ign.com/bomberman\n").is_err());
        assert!(Overrides::parse("wikipedia: [unclosed\n").is_err());
    }
}
