//! Stream filters for the PDF text extractor beyond Flate and ASCIIHex:
//! ASCII85, LZW, RunLength and the PNG/TIFF predictors. Every decoder stops
//! once its output would pass `limit` bytes, so a crafted stream cannot grow
//! without bound.

/// Why one filter step failed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FilterError {
    /// Output would exceed the caller's byte limit.
    TooLarge,
    /// The data is not valid for the filter.
    Corrupt(&'static str),
    /// Valid parameters this extractor does not implement.
    Unsupported(String),
}

fn push(out: &mut Vec<u8>, byte: u8, limit: usize) -> Result<(), FilterError> {
    if out.len() >= limit {
        return Err(FilterError::TooLarge);
    }
    out.push(byte);
    Ok(())
}

/// ASCII base-85 text, optionally wrapped as `<~ ... ~>`.
pub(super) fn ascii85(data: &[u8], limit: usize) -> Result<Vec<u8>, FilterError> {
    let mut body = data;
    while let Some((first, rest)) = body.split_first() {
        if first.is_ascii_whitespace() || *first == 0 {
            body = rest;
        } else {
            break;
        }
    }
    if let Some(rest) = body.strip_prefix(b"<~") {
        body = rest;
    }
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut filled = 0usize;
    for &byte in body {
        match byte {
            b'~' => break,
            b if b.is_ascii_whitespace() || b == 0 => continue,
            b'z' if filled == 0 => {
                for _ in 0..4 {
                    push(&mut out, 0, limit)?;
                }
            }
            b'!'..=b'u' => {
                group[filled] = byte - b'!';
                filled += 1;
                if filled == 5 {
                    let value = group
                        .iter()
                        .fold(0u64, |acc, digit| acc * 85 + u64::from(*digit));
                    let value = u32::try_from(value)
                        .map_err(|_| FilterError::Corrupt("ASCII85 group overflows 32 bits"))?;
                    for part in value.to_be_bytes() {
                        push(&mut out, part, limit)?;
                    }
                    filled = 0;
                }
            }
            _ => return Err(FilterError::Corrupt("invalid ASCII85 character")),
        }
    }
    if filled == 1 {
        return Err(FilterError::Corrupt("truncated ASCII85 group"));
    }
    if filled > 1 {
        for slot in group.iter_mut().skip(filled) {
            *slot = 84;
        }
        let value = group
            .iter()
            .fold(0u64, |acc, digit| acc * 85 + u64::from(*digit));
        let value = u32::try_from(value)
            .map_err(|_| FilterError::Corrupt("ASCII85 group overflows 32 bits"))?;
        for part in value.to_be_bytes().iter().take(filled - 1) {
            push(&mut out, *part, limit)?;
        }
    }
    Ok(out)
}

/// Run-length encoding: literal runs, repeat runs and the 128 end marker.
pub(super) fn run_length(data: &[u8], limit: usize) -> Result<Vec<u8>, FilterError> {
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(&length) = data.get(index) {
        index += 1;
        match length {
            128 => break,
            0..=127 => {
                let count = usize::from(length) + 1;
                let run = data
                    .get(index..(index + count).min(data.len()))
                    .unwrap_or_default();
                for byte in run {
                    push(&mut out, *byte, limit)?;
                }
                index += count;
            }
            _ => {
                let Some(&byte) = data.get(index) else {
                    break;
                };
                index += 1;
                for _ in 0..(257 - usize::from(length)) {
                    push(&mut out, byte, limit)?;
                }
            }
        }
    }
    Ok(out)
}

/// LZW with 9-12 bit codes, most significant bit first. `early_change` is the
/// PDF `/EarlyChange` parameter (default 1).
pub(super) fn lzw(data: &[u8], early_change: bool, limit: usize) -> Result<Vec<u8>, FilterError> {
    const CLEAR: u32 = 256;
    const END: u32 = 257;
    let early = usize::from(early_change);
    // Entry `code - 258`: (prefix code, appended byte).
    let mut table: Vec<(u16, u8)> = Vec::new();
    let mut out = Vec::new();
    let mut width = 9usize;
    let mut previous: Option<u32> = None;
    let mut bits = 0u32;
    let mut have = 0usize;
    let mut scratch: Vec<u8> = Vec::new();
    let expand = |code: u32, table: &[(u16, u8)], scratch: &mut Vec<u8>| {
        scratch.clear();
        let mut current = code;
        while current >= 258 {
            let (prefix, byte) = table[(current - 258) as usize];
            scratch.push(byte);
            current = u32::from(prefix);
        }
        scratch.push(current as u8);
        scratch.reverse();
    };
    let mut input = data.iter();
    loop {
        while have < width {
            match input.next() {
                Some(byte) => {
                    bits = (bits << 8) | u32::from(*byte);
                    have += 8;
                }
                None => return Ok(out),
            }
        }
        let code = (bits >> (have - width)) & ((1 << width) - 1);
        have -= width;
        bits &= (1u32 << have) - 1;
        if code == CLEAR {
            table.clear();
            width = 9;
            previous = None;
            continue;
        }
        if code == END {
            break;
        }
        let next = 258 + table.len() as u32;
        let first;
        if code < 256 || (code >= 258 && code < next) {
            expand(code, &table, &mut scratch);
            first = scratch[0];
        } else if code == next && previous.is_some() {
            expand(previous.unwrap_or(0), &table, &mut scratch);
            first = scratch[0];
            scratch.push(first);
        } else {
            return Err(FilterError::Corrupt("invalid LZW code"));
        }
        for byte in &scratch {
            push(&mut out, *byte, limit)?;
        }
        if let Some(previous) = previous
            && table.len() < 4096 - 258
        {
            table.push((previous as u16, first));
        }
        previous = Some(code);
        // The next code to be assigned; the width grows once it (plus the
        // early-change bias) reaches a power of two, as in libtiff and pdf.js.
        let used = 258 + table.len() + early;
        width = if used >= 2048 {
            12
        } else if used >= 1024 {
            11
        } else if used >= 512 {
            10
        } else {
            9
        };
    }
    Ok(out)
}

/// `/Predictor`, `/Colors`, `/BitsPerComponent` and `/Columns` of a
/// `DecodeParms` dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Predictor {
    pub(super) predictor: u32,
    pub(super) colors: usize,
    pub(super) bits: usize,
    pub(super) columns: usize,
}

impl Default for Predictor {
    fn default() -> Self {
        Self {
            predictor: 1,
            colors: 1,
            bits: 8,
            columns: 1,
        }
    }
}

const MAX_ROW_BYTES: usize = 64 * 1024 * 1024;

/// Undo the predictor in place of a decoded Flate or LZW stream.
pub(super) fn unpredict(mut data: Vec<u8>, params: Predictor) -> Result<Vec<u8>, FilterError> {
    if params.predictor <= 1 {
        return Ok(data);
    }
    let colors = params.colors.clamp(1, 32);
    let bits = params.bits;
    if !matches!(bits, 1 | 2 | 4 | 8 | 16) {
        return Err(FilterError::Unsupported(format!(
            "predictor with {bits}-bit components"
        )));
    }
    let row_bits = colors
        .checked_mul(bits)
        .and_then(|per_pixel| per_pixel.checked_mul(params.columns.max(1)))
        .ok_or(FilterError::Corrupt("predictor row size overflows"))?;
    let row_bytes = row_bits.div_ceil(8);
    if row_bytes > MAX_ROW_BYTES {
        return Err(FilterError::TooLarge);
    }
    let pixel_bytes = (colors * bits).div_ceil(8).max(1);
    if params.predictor == 2 {
        if bits != 8 && bits != 16 {
            return Err(FilterError::Unsupported(format!(
                "TIFF predictor with {bits}-bit components"
            )));
        }
        for row in data.chunks_mut(row_bytes) {
            if bits == 8 {
                for index in colors..row.len() {
                    row[index] = row[index].wrapping_add(row[index - colors]);
                }
            } else {
                let step = colors * 2;
                let mut index = step;
                while index + 1 < row.len() {
                    let left = u16::from_be_bytes([row[index - step], row[index - step + 1]]);
                    let here = u16::from_be_bytes([row[index], row[index + 1]]);
                    let sum = here.wrapping_add(left).to_be_bytes();
                    row[index] = sum[0];
                    row[index + 1] = sum[1];
                    index += 2;
                }
            }
        }
        return Ok(data);
    }
    if params.predictor < 10 {
        return Err(FilterError::Unsupported(format!(
            "predictor {}",
            params.predictor
        )));
    }
    let stride = row_bytes + 1;
    let mut out = Vec::with_capacity(data.len());
    let mut prior = vec![0u8; row_bytes];
    for chunk in data.chunks_mut(stride) {
        let Some((filter, row)) = chunk.split_first_mut() else {
            continue;
        };
        for index in 0..row.len() {
            let left = if index >= pixel_bytes {
                row[index - pixel_bytes]
            } else {
                0
            };
            let up = prior[index];
            let upper_left = if index >= pixel_bytes {
                prior[index - pixel_bytes]
            } else {
                0
            };
            let add = match *filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                4 => paeth(left, up, upper_left),
                _ => return Err(FilterError::Corrupt("invalid PNG predictor row filter")),
            };
            row[index] = row[index].wrapping_add(add);
        }
        out.extend_from_slice(row);
        prior[..row.len()].copy_from_slice(row);
    }
    Ok(out)
}

fn paeth(left: u8, up: u8, upper_left: u8) -> u8 {
    let (a, b, c) = (i32::from(left), i32::from(up), i32::from(upper_left));
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        upper_left
    }
}

/// Reference encoder used to prove the decoder across code-width changes.
#[cfg(test)]
pub(crate) fn lzw_encode(data: &[u8], early_change: bool) -> Vec<u8> {
    use std::collections::HashMap;
    let early = usize::from(early_change);
    let mut out = Vec::new();
    let (mut acc, mut nbits) = (0u32, 0usize);
    let mut emit = |code: u32, width: usize, out: &mut Vec<u8>| {
        acc = (acc << width) | code;
        nbits += width;
        while nbits >= 8 {
            out.push((acc >> (nbits - 8)) as u8);
            nbits -= 8;
            acc &= (1 << nbits) - 1;
        }
    };
    let mut dict: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut next = 258u32;
    let width_for = |next: u32| {
        // The decoder lags the encoder by one table entry.
        let used = next as usize - 1 + early;
        if used >= 2048 {
            12
        } else if used >= 1024 {
            11
        } else if used >= 512 {
            10
        } else {
            9
        }
    };
    let mut width = 9;
    emit(256, width, &mut out);
    let mut current: Vec<u8> = Vec::new();
    for &byte in data {
        let mut candidate = current.clone();
        candidate.push(byte);
        if candidate.len() == 1 || dict.contains_key(&candidate) {
            current = candidate;
            continue;
        }
        let code = if current.len() == 1 {
            u32::from(current[0])
        } else {
            dict[&current]
        };
        emit(code, width, &mut out);
        dict.insert(candidate, next);
        next += 1;
        width = width_for(next);
        if next >= 4094 {
            emit(256, width, &mut out);
            dict.clear();
            next = 258;
            width = 9;
        }
        current = vec![byte];
    }
    if !current.is_empty() {
        let code = if current.len() == 1 {
            u32::from(current[0])
        } else {
            dict[&current]
        };
        emit(code, width, &mut out);
    }
    emit(257, width, &mut out);
    if nbits > 0 {
        out.push((acc << (8 - nbits)) as u8);
    }
    out
}

#[cfg(test)]
pub(crate) fn ascii85_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        let mut value = u32::from_be_bytes(word);
        if chunk.len() == 4 && value == 0 {
            out.push(b'z');
            continue;
        }
        let mut digits = [0u8; 5];
        for digit in digits.iter_mut().rev() {
            *digit = (value % 85) as u8 + b'!';
            value /= 85;
        }
        out.extend_from_slice(&digits[..chunk.len() + 1]);
    }
    out.extend_from_slice(b"~>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{ascii85_encode, lzw_encode};

    #[test]
    fn ascii85_decodes_groups_zero_shorthand_and_partial_tail() {
        assert_eq!(
            ascii85(b"87cURD]i,\"Ebo80~>", 100).unwrap(),
            b"Hello World!"
        );
        for len in 0..12 {
            let data: Vec<u8> = (0..len).map(|n| (n * 37) as u8).collect();
            assert_eq!(ascii85(&ascii85_encode(&data), 100).unwrap(), data);
        }
        assert_eq!(ascii85(b"<~z87cURD]~>", 100).unwrap(), b"\0\0\0\0Hello");
        assert_eq!(
            ascii85(b"87\n cURD\t]i,\"Ebo80", 100).unwrap(),
            b"Hello World!"
        );
        assert_eq!(ascii85(b"87cUR~>", 100).unwrap(), b"Hell");
        assert_eq!(ascii85(b"~>", 100).unwrap(), b"");
        assert!(ascii85(b"87cU\x7f", 100).is_err());
        assert!(ascii85(b"uuuuu", 100).is_err(), "overflowing group");
        assert_eq!(ascii85(b"zzzz", 15), Err(FilterError::TooLarge));
    }

    #[test]
    fn run_length_decodes_literals_repeats_and_stops_at_eod() {
        let data = [2, b'a', b'b', b'c', 254, b'x', 128, 0, b'z'];
        assert_eq!(run_length(&data, 100).unwrap(), b"abcxxx");
        assert_eq!(run_length(&[255, b'q'], 1), Err(FilterError::TooLarge));
        assert_eq!(run_length(&[5, b'a'], 100).unwrap(), b"a", "truncated run");
    }

    #[test]
    fn lzw_decodes_the_pdf_specification_example() {
        let encoded = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        let expected = [45, 45, 45, 45, 45, 65, 45, 45, 45, 66];
        assert_eq!(lzw(&encoded, true, 100).unwrap(), expected);
        assert_eq!(lzw(&encoded, true, 4), Err(FilterError::TooLarge));
        assert!(lzw(&[0xFF, 0xFF, 0xFF], true, 100).is_err());
    }

    #[test]
    fn lzw_round_trips_data_that_grows_the_code_width() {
        let mut data = Vec::new();
        let mut state = 12345u32;
        for _ in 0..20_000 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            data.push(b'a' + ((state >> 16) % 7) as u8);
        }
        for early in [true, false] {
            let encoded = lzw_encode(&data, early);
            assert_eq!(
                lzw(&encoded, early, 1 << 20).unwrap(),
                data,
                "early={early}"
            );
        }
    }

    #[test]
    fn lzw_decodes_an_independent_early_change_zero_stream() {
        // 64x64 palette image encoded by PIL's GIF writer. GIF LZW has no
        // early change, so the codes (re-packed most significant bit first,
        // with the widths the GIF decoder uses) are an independent
        // EarlyChange 0 stream crossing 9->10->11->12 bit widths.
        let strip = include_bytes!("fixtures/gif_lzw_early0_strip.bin");
        let mut state = 987_654_321u32;
        let expected: Vec<u8> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (state >> 16) as u8
            })
            .collect();
        assert_eq!(lzw(strip, false, 1 << 20).unwrap(), expected);
        assert_ne!(lzw(strip, true, 1 << 20).ok(), Some(expected));
    }

    #[test]
    fn lzw_decodes_a_real_libtiff_strip() {
        // 64x64 gray strip written by PIL/libtiff (tiff_lzw, EarlyChange 1
        // semantics), passing through the 9->10->11->12 bit width changes.
        let strip = include_bytes!("fixtures/pil_tiff_lzw_strip.bin");
        let mut state = 987_654_321u32;
        let expected: Vec<u8> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (state >> 16) as u8
            })
            .collect();
        assert_eq!(lzw(strip, true, 1 << 20).unwrap(), expected);
    }

    #[test]
    fn png_predictors_reverse_every_row_filter() {
        // Two rows of 4 gray pixels per filter type, built by applying each
        // forward filter to known pixels.
        let rows: [[u8; 4]; 2] = [[10, 20, 30, 40], [15, 18, 50, 41]];
        let mut encoded = Vec::new();
        let mut prior = [0u8; 4];
        for (n, row) in rows.iter().enumerate() {
            let filter = (n * 2 + 1) as u8; // sub (1) then avg (3)
            encoded.push(filter);
            for i in 0..4 {
                let left = if i > 0 { row[i - 1] } else { 0 };
                let predicted = match filter {
                    1 => left,
                    _ => ((u16::from(left) + u16::from(prior[i])) / 2) as u8,
                };
                encoded.push(row[i].wrapping_sub(predicted));
            }
            prior = *row;
        }
        let params = Predictor {
            predictor: 15,
            colors: 1,
            bits: 8,
            columns: 4,
        };
        let decoded = unpredict(encoded, params).unwrap();
        assert_eq!(decoded, [10, 20, 30, 40, 15, 18, 50, 41]);
        // Up and Paeth rows.
        let encoded = vec![2, 1, 2, 3, 4, 4, 1, 1, 1, 1];
        let decoded = unpredict(encoded, params).unwrap();
        assert_eq!(&decoded[..4], &[1, 2, 3, 4]);
        let mut expect = [0u8; 4];
        for i in 0..4 {
            let left = if i > 0 { expect[i - 1] } else { 0 };
            let up = [1u8, 2, 3, 4][i];
            let ul = if i > 0 { [1u8, 2, 3, 4][i - 1] } else { 0 };
            expect[i] = 1u8.wrapping_add(paeth(left, up, ul));
        }
        assert_eq!(&decoded[4..], &expect);
        assert!(unpredict(vec![9, 0, 0, 0, 0], params).is_err());
    }

    #[test]
    fn tiff_predictor_accumulates_horizontal_differences() {
        let params = Predictor {
            predictor: 2,
            colors: 1,
            bits: 8,
            columns: 4,
        };
        assert_eq!(
            unpredict(vec![1, 1, 1, 1, 5, 0, 0, 255], params).unwrap(),
            [1, 2, 3, 4, 5, 5, 5, 4]
        );
        let sixteen = Predictor {
            bits: 16,
            columns: 2,
            ..params
        };
        assert_eq!(unpredict(vec![0, 1, 0, 1], sixteen).unwrap(), [0, 1, 0, 2]);
        let odd = Predictor { bits: 4, ..params };
        assert!(matches!(
            unpredict(vec![0; 4], odd),
            Err(FilterError::Unsupported(_))
        ));
    }

    #[test]
    fn predictor_rejects_unsupported_and_oversized_shapes() {
        let huge = Predictor {
            predictor: 12,
            colors: 32,
            bits: 16,
            columns: usize::MAX / 8,
        };
        assert!(unpredict(vec![0; 8], huge).is_err());
        let legacy = Predictor {
            predictor: 5,
            ..Predictor::default()
        };
        assert!(matches!(
            unpredict(vec![0], legacy),
            Err(FilterError::Unsupported(_))
        ));
    }
}
