//! Glyph-name decoding for PDF simple-font `/Differences` encodings: a compact
//! subset of the Adobe Glyph List plus `StandardEncoding`.

/// Text for an Adobe glyph name, or `None` when the name is unknown. Ligatures
/// expand to their ASCII letters so the model sees `fi`, not `U+FB01`.
pub(super) fn glyph_text(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    // `f_i` style ligature components and `.alt` style suffixes.
    let name = name.split('.').next().unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    if name.contains('_') {
        let mut out = String::new();
        for part in name.split('_') {
            out.push_str(&glyph_text(part.as_bytes())?);
        }
        return Some(out);
    }
    if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
        return Some(name.to_owned());
    }
    if let Some(hex) = name.strip_prefix("uni")
        && hex.len() >= 4
        && hex.len().is_multiple_of(4)
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let mut out = String::new();
        for chunk in hex.as_bytes().chunks(4) {
            let value = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
            out.push(char::from_u32(value)?);
        }
        return Some(out);
    }
    if let Some(hex) = name.strip_prefix('u')
        && (4..=6).contains(&hex.len())
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let value = u32::from_str_radix(hex, 16).ok()?;
        return char::from_u32(value).map(String::from);
    }
    NAMES
        .binary_search_by(|(known, _)| (*known).cmp(name))
        .ok()
        .map(|index| NAMES[index].1.to_owned())
}

/// Adobe `StandardEncoding` for one code (the default of non-symbolic Type 1
/// fonts without an `/Encoding`).
pub(super) fn standard_encoding(code: u8) -> Option<char> {
    Some(match code {
        0x27 => '\u{2019}',
        0x60 => '\u{2018}',
        0x20..=0x7e => char::from(code),
        0xa1 => '¡',
        0xa2 => '¢',
        0xa3 => '£',
        0xa4 => '\u{2044}',
        0xa5 => '¥',
        0xa6 => 'ƒ',
        0xa7 => '§',
        0xa8 => '¤',
        0xa9 => '\'',
        0xaa => '“',
        0xab => '«',
        0xac => '‹',
        0xad => '›',
        0xae | 0xaf => return None,
        0xb1 => '–',
        0xb2 => '†',
        0xb3 => '‡',
        0xb4 => '·',
        0xb6 => '¶',
        0xb7 => '•',
        0xb8 => '‚',
        0xb9 => '„',
        0xba => '”',
        0xbb => '»',
        0xbc => '…',
        0xbd => '‰',
        0xbf => '¿',
        0xc1 => '`',
        0xc2 => '´',
        0xc3 => 'ˆ',
        0xc4 => '˜',
        0xc5 => '¯',
        0xc6 => '˘',
        0xc7 => '˙',
        0xc8 => '¨',
        0xca => '˚',
        0xcb => '¸',
        0xcd => '˝',
        0xce => '˛',
        0xcf => 'ˇ',
        0xd0 => '—',
        0xe1 => 'Æ',
        0xe3 => 'ª',
        0xe8 => 'Ł',
        0xe9 => 'Ø',
        0xea => 'Œ',
        0xeb => 'º',
        0xf1 => 'æ',
        0xf5 => 'ı',
        0xf8 => 'ł',
        0xf9 => 'ø',
        0xfa => 'œ',
        0xfb => 'ß',
        0x09 | 0x0a | 0x0d => char::from(code),
        _ => return None,
    })
}

/// `StandardEncoding` ligature codes expand to two letters.
pub(super) fn standard_ligature(code: u8) -> Option<&'static str> {
    match code {
        0xae => Some("fi"),
        0xaf => Some("fl"),
        _ => None,
    }
}

/// Sorted by name for binary search.
const NAMES: &[(&str, &str)] = &[
    ("A", "A"),
    ("AE", "Æ"),
    ("Aacute", "Á"),
    ("Acircumflex", "Â"),
    ("Adieresis", "Ä"),
    ("Agrave", "À"),
    ("Aring", "Å"),
    ("Atilde", "Ã"),
    ("Ccedilla", "Ç"),
    ("Delta", "Δ"),
    ("Eacute", "É"),
    ("Ecircumflex", "Ê"),
    ("Edieresis", "Ë"),
    ("Egrave", "È"),
    ("Eth", "Ð"),
    ("Euro", "€"),
    ("Iacute", "Í"),
    ("Icircumflex", "Î"),
    ("Idieresis", "Ï"),
    ("Igrave", "Ì"),
    ("Lslash", "Ł"),
    ("Ntilde", "Ñ"),
    ("OE", "Œ"),
    ("Oacute", "Ó"),
    ("Ocircumflex", "Ô"),
    ("Odieresis", "Ö"),
    ("Ograve", "Ò"),
    ("Omega", "Ω"),
    ("Oslash", "Ø"),
    ("Otilde", "Õ"),
    ("Scaron", "Š"),
    ("Thorn", "Þ"),
    ("Uacute", "Ú"),
    ("Ucircumflex", "Û"),
    ("Udieresis", "Ü"),
    ("Ugrave", "Ù"),
    ("Yacute", "Ý"),
    ("Ydieresis", "Ÿ"),
    ("Zcaron", "Ž"),
    ("aacute", "á"),
    ("acircumflex", "â"),
    ("acute", "´"),
    ("adieresis", "ä"),
    ("ae", "æ"),
    ("agrave", "à"),
    ("alpha", "α"),
    ("ampersand", "&"),
    ("approxequal", "≈"),
    ("aring", "å"),
    ("asciicircum", "^"),
    ("asciitilde", "~"),
    ("asterisk", "*"),
    ("at", "@"),
    ("atilde", "ã"),
    ("backslash", "\\"),
    ("bar", "|"),
    ("beta", "β"),
    ("braceleft", "{"),
    ("braceright", "}"),
    ("bracketleft", "["),
    ("bracketright", "]"),
    ("breve", "˘"),
    ("brokenbar", "¦"),
    ("bullet", "•"),
    ("caron", "ˇ"),
    ("ccedilla", "ç"),
    ("cedilla", "¸"),
    ("cent", "¢"),
    ("circumflex", "ˆ"),
    ("colon", ":"),
    ("comma", ","),
    ("copyright", "©"),
    ("currency", "¤"),
    ("dagger", "†"),
    ("daggerdbl", "‡"),
    ("degree", "°"),
    ("delta", "δ"),
    ("dieresis", "¨"),
    ("divide", "÷"),
    ("dollar", "$"),
    ("dotaccent", "˙"),
    ("dotlessi", "ı"),
    ("eacute", "é"),
    ("ecircumflex", "ê"),
    ("edieresis", "ë"),
    ("egrave", "è"),
    ("eight", "8"),
    ("ellipsis", "…"),
    ("emdash", "—"),
    ("endash", "–"),
    ("epsilon", "ε"),
    ("equal", "="),
    ("eth", "ð"),
    ("exclam", "!"),
    ("exclamdown", "¡"),
    ("ff", "ff"),
    ("ffi", "ffi"),
    ("ffl", "ffl"),
    ("fi", "fi"),
    ("five", "5"),
    ("fl", "fl"),
    ("florin", "ƒ"),
    ("four", "4"),
    ("fraction", "⁄"),
    ("gamma", "γ"),
    ("germandbls", "ß"),
    ("grave", "`"),
    ("greater", ">"),
    ("greaterequal", "≥"),
    ("guillemotleft", "«"),
    ("guillemotright", "»"),
    ("guilsinglleft", "‹"),
    ("guilsinglright", "›"),
    ("hungarumlaut", "˝"),
    ("hyphen", "-"),
    ("iacute", "í"),
    ("icircumflex", "î"),
    ("idieresis", "ï"),
    ("igrave", "ì"),
    ("infinity", "∞"),
    ("lambda", "λ"),
    ("less", "<"),
    ("lessequal", "≤"),
    ("logicalnot", "¬"),
    ("lslash", "ł"),
    ("macron", "¯"),
    ("minus", "−"),
    ("mu", "µ"),
    ("multiply", "×"),
    ("nine", "9"),
    ("notequal", "≠"),
    ("ntilde", "ñ"),
    ("numbersign", "#"),
    ("oacute", "ó"),
    ("ocircumflex", "ô"),
    ("odieresis", "ö"),
    ("oe", "œ"),
    ("ogonek", "˛"),
    ("ograve", "ò"),
    ("omega", "ω"),
    ("one", "1"),
    ("onehalf", "½"),
    ("onequarter", "¼"),
    ("onesuperior", "¹"),
    ("ordfeminine", "ª"),
    ("ordmasculine", "º"),
    ("oslash", "ø"),
    ("otilde", "õ"),
    ("paragraph", "¶"),
    ("parenleft", "("),
    ("parenright", ")"),
    ("percent", "%"),
    ("period", "."),
    ("periodcentered", "·"),
    ("perthousand", "‰"),
    ("phi", "φ"),
    ("pi", "π"),
    ("plus", "+"),
    ("plusminus", "±"),
    ("question", "?"),
    ("questiondown", "¿"),
    ("quotedbl", "\""),
    ("quotedblbase", "„"),
    ("quotedblleft", "“"),
    ("quotedblright", "”"),
    ("quoteleft", "‘"),
    ("quoteright", "’"),
    ("quotesinglbase", "‚"),
    ("quotesingle", "'"),
    ("registered", "®"),
    ("ring", "˚"),
    ("scaron", "š"),
    ("section", "§"),
    ("semicolon", ";"),
    ("seven", "7"),
    ("sigma", "σ"),
    ("six", "6"),
    ("slash", "/"),
    ("space", " "),
    ("sterling", "£"),
    ("summation", "∑"),
    ("tau", "τ"),
    ("thorn", "þ"),
    ("three", "3"),
    ("threequarters", "¾"),
    ("threesuperior", "³"),
    ("tilde", "˜"),
    ("trademark", "™"),
    ("two", "2"),
    ("twosuperior", "²"),
    ("uacute", "ú"),
    ("ucircumflex", "û"),
    ("udieresis", "ü"),
    ("ugrave", "ù"),
    ("underscore", "_"),
    ("yacute", "ý"),
    ("ydieresis", "ÿ"),
    ("yen", "¥"),
    ("zcaron", "ž"),
    ("zero", "0"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_table_is_sorted_and_unique() {
        for pair in NAMES.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} !< {}", pair[0].0, pair[1].0);
        }
    }

    #[test]
    fn glyph_names_decode() {
        assert_eq!(glyph_text(b"fi").as_deref(), Some("fi"));
        assert_eq!(glyph_text(b"eacute").as_deref(), Some("é"));
        assert_eq!(glyph_text(b"endash").as_deref(), Some("–"));
        assert_eq!(glyph_text(b"uni00E9").as_deref(), Some("é"));
        assert_eq!(glyph_text(b"u1F600").as_deref(), Some("\u{1F600}"));
        assert_eq!(glyph_text(b"f_i").as_deref(), Some("fi"));
        assert_eq!(glyph_text(b"a.sc").as_deref(), Some("a"));
        assert_eq!(glyph_text(b"g123"), None);
    }
}
