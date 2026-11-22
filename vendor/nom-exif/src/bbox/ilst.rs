use nom::bytes::complete::{tag, take};
use nom::combinator::{fail, map_res};
use nom::error::context;
use nom::multi::many0;
use nom::number::complete::{
    be_f32, be_f64, be_i16, be_i24, be_i32, be_i64, be_u16, be_u24, be_u32, be_u64, u8,
};
use nom::Parser;

use crate::error::{nom_err_to_malformed, MalformedKind};
use crate::EntryValue;

use super::BoxHeader;

/// Represents an [item list atom][1].
///
/// ilst is not a fullbox, it doesn't have version & flags.
///
/// atom-path: moov/meta/ilst
///
/// [1]: https://developer.apple.com/documentation/quicktime-file-format/metadata_item_list_atom
#[derive(Debug, Clone, PartialEq)]
pub struct IlstBox {
    header: BoxHeader,
    pub items: Vec<IlstItem>,
}

impl IlstBox {
    pub fn parse_box(input: &[u8]) -> nom::IResult<&[u8], IlstBox> {
        let (remain, header) = BoxHeader::parse(input)?;
        let (remain, items) = many0(IlstItem::parse).parse(remain)?;

        Ok((remain, IlstBox { header, items }))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IlstItem {
    size: u32,
    index: u32,    // 1-based index (start from 1)
    data_len: u32, // including self size

    /// Type indicator, see [type
    /// indicator](https://developer.apple.com/documentation/quicktime-file-format/type_indicator)
    type_set: u8,
    type_code: u32, // 24-bits

    local: u32,
    pub value: EntryValue, // len: data_len - 16
}

impl IlstItem {
    fn parse<'a>(input: &'a [u8]) -> nom::IResult<&'a [u8], IlstItem> {
        let (remain, (size, index, data_len, _, type_set, type_code, local)) =
            (be_u32, be_u32, be_u32, tag("data"), u8, be_u24, be_u32).parse(input)?;

        if size < 24 || data_len < 16 {
            context("invalid ilst item", fail::<_, (), _>()).parse(remain)?;
        }

        // assert_eq!(size - 24, data_len - 16);
        if size - 24 != data_len - 16 {
            context("invalid ilst item", fail::<_, (), _>()).parse(remain)?;
        }

        let (remain, value) = map_res(take(data_len - 16), |bs: &'a [u8]| {
            parse_value(type_code, bs)
        })
        .parse(remain)?;

        Ok((
            remain,
            IlstItem {
                size,
                index,
                data_len,
                type_set,
                type_code,
                local,
                value,
            },
        ))
    }
}

/// Parse ilst item data to value, see [Well-known
/// types](https://developer.apple.com/documentation/quicktime-file-format/well-known_types)
#[tracing::instrument(skip(data))]
fn parse_value(type_code: u32, data: &[u8]) -> crate::Result<EntryValue> {
    use EntryValue::*;
    let v = match type_code {
        1 => {
            let s = String::from_utf8(data.to_vec()).map_err(|e| crate::Error::Malformed {
                kind: MalformedKind::IsoBmffBox,
                message: format!("ilst text item is not valid utf-8: {e}"),
            })?;
            Text(s)
        }
        21 => match data.len() {
            1 => data[0].into(),
            2 => be_i16(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            3 => be_i24(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            4 => be_i32(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            8 => be_i64(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            data_len => {
                let data_type = "BE Signed Integer";
                tracing::warn!(data_type, data_len, "Invalid ilst item data.");
                let msg = format!(
                    "Invalid ilst item data; \
                    data type is {data_type} while data len is : {data_len}",
                );
                return Err(crate::Error::Malformed {
                    kind: MalformedKind::IsoBmffBox,
                    message: msg,
                });
            }
        },
        22 => match data.len() {
            1 => data[0].into(),
            2 => be_u16(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            3 => be_u24(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            4 => be_u32(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            8 => be_u64(data)
                .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
                .1
                .into(),
            data_len => {
                let data_type = "BE Unsigned Integer";
                tracing::warn!(data_type, data_len, "Invalid ilst item data.");
                let msg = format!(
                    "Invalid ilst item data; \
                    data type is {data_type} while data len is : {data_len}",
                );
                return Err(crate::Error::Malformed {
                    kind: MalformedKind::IsoBmffBox,
                    message: msg,
                });
            }
        },
        23 => be_f32(data)
            .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
            .1
            .into(),
        24 => be_f64(data)
            .map_err(|e| nom_err_to_malformed(e, MalformedKind::IsoBmffBox))?
            .1
            .into(),
        data_type => {
            let msg = "Unsupported ilst item data type";
            tracing::warn!(data_type, "{}.", msg);
            return Err(crate::Error::Malformed {
                kind: MalformedKind::IsoBmffBox,
                message: format!("{}: {data_type}", msg),
            });
        }
    };
    Ok(v)
}
