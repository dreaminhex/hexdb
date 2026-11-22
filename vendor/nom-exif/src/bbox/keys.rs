use nom::bytes::complete::take;
use nom::combinator::{flat_map, map_res};
use nom::multi::many_m_n;
use nom::number::complete::be_u32;
use nom::Parser;

use crate::bbox::{FullBoxHeader, ParseBody};

/// Represents a [keys atom][1].
///
/// `keys` is a fullbox which contains version & flags.
///
/// atom-path: moov/meta/keys
///
/// [1]: https://developer.apple.com/documentation/quicktime-file-format/metadata_item_keys_atom
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeysBox {
    header: FullBoxHeader,
    entry_count: u32,
    pub entries: Vec<KeyEntry>,
}

impl ParseBody<KeysBox> for KeysBox {
    fn parse_body(body: &[u8], header: FullBoxHeader) -> nom::IResult<&[u8], KeysBox> {
        let (remain, entry_count) = be_u32(body)?;
        let (remain, entries) =
            many_m_n(entry_count as usize, entry_count as usize, KeyEntry::parse).parse(remain)?;

        Ok((
            remain,
            KeysBox {
                header,
                entry_count,
                entries,
            },
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyEntry {
    size: u32,
    pub namespace: String, // 4 bytes
    pub key: String,       // len: size - 8
}

impl KeyEntry {
    fn parse<'a>(input: &'a [u8]) -> nom::IResult<&'a [u8], KeyEntry> {
        let (remain, s) = map_res(
            flat_map(
                map_res(be_u32, |len| {
                    len.checked_sub(4).ok_or("invalid KeyEntry header")
                }),
                take,
            ),
            |bs: &'a [u8]| String::from_utf8(bs.to_vec()),
        )
        .parse(input)?;

        Ok((
            remain,
            KeyEntry {
                size: (s.len() + 4) as u32,
                namespace: s.chars().take(4).collect(),
                key: s.chars().skip(4).collect(),
            },
        ))
    }
}
