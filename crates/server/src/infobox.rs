//! Fields of a Wikipedia article's video game infobox, from its wikitext:
//! used for what Wikidata lacks (developers, publishers, genres, modes).
//!
//! ```text
//! | developer = Gigawatt Studios<br />[[The Collective (company)|The Collective]] (PS1)
//! | publisher = {{vgrelease|NA|[[SouthPeak Interactive]]|EU|[[Gremlin Interactive]]}}
//! ```

/// The raw value of an infobox field: from `| name =` to the next field.
pub fn field(wikitext: &str, name: &str) -> Option<String> {
    let start = wikitext.to_ascii_lowercase().find("{{infobox video game")?;
    let mut lines = wikitext[start..].lines().skip(1);
    let mut value: Option<String> = None;
    for line in lines.by_ref() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("}}") && value.is_none() {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix('|') {
            if value.is_some() {
                break;
            }
            let Some((key, v)) = rest.split_once('=') else { continue };
            if key.trim().eq_ignore_ascii_case(name) {
                value = Some(v.trim().to_owned());
            }
        } else if trimmed.starts_with("}}") {
            break;
        } else if let Some(v) = value.as_mut() {
            // A value spanning lines (lists).
            v.push('\n');
            v.push_str(trimmed);
        }
    }
    value.filter(|v| !v.is_empty())
}

/// The names a field lists, as plain text. When some are marked for a
/// platform ("(PS1)"), only the PlayStation's are kept.
pub fn values(raw: &str) -> Vec<String> {
    let text = expand_templates(&links(&strip_tags(raw)));
    let text = text
        .replace("<br />", "\n")
        .replace("<br/>", "\n")
        .replace("<br>", "\n")
        .replace("'''", "")
        .replace("''", "");
    let items: Vec<String> = text
        .split(['\n', '*'])
        .map(|s| s.trim().trim_end_matches(',').trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let playstation = |s: &str| {
        let s = s.to_ascii_lowercase();
        ["(ps1)", "(ps)", "(psx)", "(playstation)"]
            .iter()
            .any(|p| s.contains(p))
    };
    let tagged = items.iter().any(|s| playstation(s));
    let mut out: Vec<String> = Vec::new();
    for item in items.iter().filter(|s| !tagged || playstation(s)) {
        // "The Collective (PS1)" → "The Collective".
        let name = match item.rfind(" (") {
            Some(i) if item.ends_with(')') => item[..i].trim(),
            _ => item.trim(),
        };
        if !name.is_empty() && !out.iter().any(|o| o == name) {
            out.push(name.to_owned());
        }
    }
    out.truncate(6);
    out
}

/// Drops references and comments.
fn strip_tags(s: &str) -> String {
    let mut out = s.to_owned();
    // <ref name=x/>, <ref ...>...</ref>, <!-- ... -->
    while let Some(start) = out.find("<ref").or_else(|| out.find("<!--")) {
        let rest = &out[start..];
        let end = if rest.starts_with("<!--") {
            rest.find("-->").map(|e| e + 3)
        } else {
            let open_end = rest.find('>');
            match open_end {
                Some(e) if rest[..e].ends_with('/') => Some(e + 1),
                Some(_) => rest.find("</ref>").map(|e| e + 6),
                None => None,
            }
        };
        let Some(end) = end else {
            out.truncate(start);
            break;
        };
        out.replace_range(start..start + end, "");
    }
    out
}

/// [[target|text]] → text; [[target]] → target.
fn links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("]]") else {
            rest = &rest[start..];
            break;
        };
        let inner = &rest[start + 2..start + end];
        out.push_str(inner.rsplit('|').next().unwrap_or(inner));
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    out
}

/// Replaces templates, innermost first: lists by their items, release
/// templates by their values (every other argument, after a region), and
/// any other by nothing.
fn expand_templates(s: &str) -> String {
    let mut out = s.to_owned();
    while let Some(end) = out.find("}}") {
        let Some(start) = out[..end].rfind("{{") else {
            out.replace_range(end..end + 2, "");
            continue;
        };
        let inner = &out[start + 2..end];
        let mut parts = inner.split('|');
        let name = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let args: Vec<&str> = parts.filter(|a| !a.contains('=')).collect();
        let replacement = match name.as_str() {
            "ubl" | "unbulleted list" | "plainlist" | "plain list" | "flatlist" | "hlist" | "collapsible list" => {
                args.join("\n")
            }
            "vgrelease" | "vgrelease new" | "video game release" | "vgrtbl" => {
                args.iter().skip(1).step_by(2).copied().collect::<Vec<_>>().join("\n")
            }
            _ => String::new(),
        };
        out.replace_range(start..end + 2, &replacement);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: &str = "{{Infobox video game
| title = Men in Black: The Game
| developer = Gigawatt Studios<br />[[The Collective (company)|The Collective]] (PS1)
| publisher = {{vgrelease|NA|[[SouthPeak Interactive]]|EU|[[Gremlin Interactive]]}}
| genre = [[Survival horror]]
| modes = [[Single-player video game|Single-player]]
| producer = David Koenig<ref name=Credits>{{cite web|title=Credits|url=http://example.com}}</ref>
| programmer = Robert Knaack<br />Dan Cotton<ref name=Credits/>
| writer =
}}";

    #[test]
    fn fields() {
        let get = |name| values(&field(MIB, name).unwrap_or_default());
        assert_eq!(get("developer"), ["The Collective"]);
        assert_eq!(get("publisher"), ["SouthPeak Interactive", "Gremlin Interactive"]);
        assert_eq!(get("genre"), ["Survival horror"]);
        assert_eq!(get("modes"), ["Single-player"]);
        assert_eq!(get("producer"), ["David Koenig"]);
        assert_eq!(get("programmer"), ["Robert Knaack", "Dan Cotton"]);
        assert!(field(MIB, "writer").is_none());
        assert!(field(MIB, "composer").is_none());
    }

    #[test]
    fn lists() {
        let text = "{{Infobox video game\n| developer = {{ubl|[[Naughty Dog]]|Mass Media}}\n| modes =\n* [[Single-player]]\n* [[Multiplayer]]\n}}";
        assert_eq!(
            values(&field(text, "developer").unwrap()),
            ["Naughty Dog", "Mass Media"]
        );
        assert_eq!(values(&field(text, "modes").unwrap()), ["Single-player", "Multiplayer"]);
    }
}
