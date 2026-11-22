//! Shift-JIS, as save titles use it: ASCII, its full-width forms, common
//! punctuation, and kana. Kanji (not mapped) show as "?".

/// Decodes up to the first NUL.
pub fn decode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            break;
        }
        if b < 0x80 {
            out.push(char::from(b));
            i += 1;
            continue;
        }
        if (0xa1..=0xdf).contains(&b) {
            // Half-width katakana.
            out.push(char::from_u32(0xff61 + u32::from(b - 0xa1)).unwrap_or('?'));
            i += 1;
            continue;
        }
        let Some(&low) = bytes.get(i + 1) else { break };
        i += 2;
        out.push(double(u16::from_be_bytes([b, low])).unwrap_or('?'));
    }
    out
}

fn double(c: u16) -> Option<char> {
    let ascii = |base: u16, first: u8| char::from(first + (c - base) as u8);
    let code = match c {
        0x8140 => ' ',
        0x8141 => '、',
        0x8142 => '。',
        0x8143 => ',',
        0x8144 => '.',
        0x8145 => '・',
        0x8146 => ':',
        0x8147 => ';',
        0x8148 => '?',
        0x8149 => '!',
        0x814a => '゛',
        0x814b => '゜',
        0x814d => '`',
        0x814f => '^',
        0x8151 => '_',
        0x815b => 'ー',
        0x815c | 0x815d => '-',
        0x815e => '/',
        0x815f => '\\',
        0x8160 => '~',
        0x8162 => '|',
        0x8163 => '…',
        0x8165 | 0x8166 => '\'',
        0x8167 | 0x8168 => '"',
        0x8169 => '(',
        0x816a => ')',
        0x816d => '[',
        0x816e => ']',
        0x816f => '{',
        0x8170 => '}',
        0x8175 => '「',
        0x8176 => '」',
        0x817b => '+',
        0x817c => '-',
        0x817e => '×',
        0x8181 => '=',
        0x8183 => '<',
        0x8184 => '>',
        0x818b => '°',
        0x818f => '¥',
        0x8190 => '$',
        0x8193 => '%',
        0x8194 => '#',
        0x8195 => '&',
        0x8196 => '*',
        0x8197 => '@',
        0x819a => '☆',
        0x819b => '★',
        0x819c => '○',
        0x819d => '●',
        0x81a0 => '□',
        0x81a1 => '■',
        0x81a2 => '△',
        0x81a3 => '▲',
        0x81a8 => '→',
        0x81a9 => '←',
        0x81aa => '↑',
        0x81ab => '↓',
        0x824f..=0x8258 => ascii(0x824f, b'0'),
        0x8260..=0x8279 => ascii(0x8260, b'A'),
        0x8281..=0x829a => ascii(0x8281, b'a'),
        0x829f..=0x82f1 => return char::from_u32(0x3041 + u32::from(c - 0x829f)),
        0x8340..=0x837e => return char::from_u32(0x30a1 + u32::from(c - 0x8340)),
        0x8380..=0x8396 => return char::from_u32(0x30e0 + u32::from(c - 0x8380)),
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn full_width_and_kana() {
        // "ＴＥＫＫＥＮ３" then "ゲーム" in katakana, "てす" in hiragana.
        let bytes = [
            0x82, 0x73, 0x82, 0x64, 0x82, 0x6a, 0x82, 0x6a, 0x82, 0x64, 0x82, 0x6d, 0x82, 0x52, 0x81, 0x40, 0x83, 0x51,
            0x81, 0x5b, 0x83, 0x80, 0x82, 0xc4, 0x82, 0xb7, 0, 0x41,
        ];
        assert_eq!(decode(&bytes), "TEKKEN3 ゲームてす");
        assert_eq!(decode(b"Plain\0junk"), "Plain");
        // An unmapped kanji.
        assert_eq!(decode(&[0x88, 0x9f]), "?");
    }
}
