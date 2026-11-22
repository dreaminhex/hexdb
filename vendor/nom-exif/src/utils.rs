use nom::{combinator::map_res, IResult, Parser};

pub(crate) fn parse_cstr(input: &[u8]) -> IResult<&[u8], String> {
    let (remain, s) = map_res(
        nom::bytes::streaming::take_till(|b| b == 0),
        |bs: &[u8]| {
            if bs.is_empty() {
                Ok("".to_owned())
            } else {
                String::from_utf8(bs.to_vec())
            }
        },
    )
    .parse(input)?;

    // consumes the zero byte
    Ok((&remain[1..], s)) // Safe-slice
}
