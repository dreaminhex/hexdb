use nom::{
    bytes::streaming::{tag, take},
    number, IResult,
};

use crate::{jpeg, utils::parse_cstr};

const MAGIC: &[u8] = b"FUJIFILMCCD-RAW ";

/// Refer to: [Fujifilm RAF](http://fileformats.archiveteam.org/wiki/Fujifilm_RAF)
#[allow(unused)]
pub struct RafInfo<'a> {
    pub version: &'a [u8],
    pub camera_num_id: &'a [u8],
    pub camera_string: String,
    pub directory_ver: &'a [u8],
    pub image_offset: u32,
    pub exif_data: Option<&'a [u8]>,
}

impl RafInfo<'_> {
    pub fn check(input: &[u8]) -> crate::Result<()> {
        // check magic
        let _ = nom::bytes::complete::tag(MAGIC)(input).map_err(|e| {
            crate::error::nom_err_to_malformed(e, crate::error::MalformedKind::TiffHeader)
        })?;
        Ok(())
    }

    pub(crate) fn parse(input: &[u8]) -> IResult<&[u8], RafInfo<'_>> {
        // magic
        let (remain, _) = tag(MAGIC)(input)?;
        let (remain, version) = take(4usize)(remain)?;
        let (remain, camera_num_id) = take(8usize)(remain)?;
        let (remain, camera_string) = take(32usize)(remain)?;
        let (remain, directory_ver) = take(4usize)(remain)?;

        // 20 bytes unknown
        let (remain, _) = take(20usize)(remain)?;

        let (remain, image_offset) = number::streaming::be_u32(remain)?;

        // skip to image_offset
        let skip_n = image_offset
            .checked_sub((input.len() - remain.len()) as u32)
            .ok_or_else(|| {
                nom::Err::Failure(nom::error::make_error(remain, nom::error::ErrorKind::Fail))
            })?;
        let (remain, _) = take(skip_n)(remain)?;

        // parse as a JPEG
        jpeg::check_jpeg(remain).map_err(|_| {
            nom::Err::Failure(nom::error::make_error(remain, nom::error::ErrorKind::Fail))
        })?;
        let (remain, exif_data) = jpeg::extract_exif_data(remain)?;

        let (_, camera_string) = parse_cstr(camera_string)?;

        Ok((
            remain,
            RafInfo {
                version,
                camera_num_id,
                camera_string,
                directory_ver,
                image_offset,
                exif_data,
            },
        ))
    }
}
