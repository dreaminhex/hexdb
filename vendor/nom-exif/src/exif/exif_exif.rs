use std::fmt::Debug;

use nom::{
    branch::alt, bytes::streaming::tag, combinator, number::Endianness, IResult, Needed, Parser,
};

use crate::{
    EntryValue, ExifEntry, ExifEntryRef, ExifIter, ExifTag, GPSInfo, IfdIndex, IfdKind, TagOrCode,
};

use super::ifd::ParsedImageFileDirectory;

/// Represents parsed Exif information, can be converted from an [`ExifIter`]
/// like this: `let exif: Exif = iter.into()`.
#[derive(Clone, Debug, PartialEq)]
pub struct Exif {
    ifds: Vec<ParsedImageFileDirectory>,
    gps_info: Option<GPSInfo>,
    errors: Vec<(IfdIndex, TagOrCode, crate::EntryError)>,
    has_embedded_track: bool,
}

impl Exif {
    fn new(gps_info: Option<GPSInfo>, has_embedded_track: bool) -> Exif {
        Exif {
            ifds: Vec::new(),
            gps_info,
            errors: Vec::new(),
            has_embedded_track,
        }
    }

    /// Get entry value for the specified `tag` in ifd0 (the main image).
    ///
    /// *Note*:
    ///
    /// - The parsing error related to this tag won't be reported by this
    ///   method. Either this entry is not parsed successfully, or the tag does
    ///   not exist in the input data, this method will return None.
    ///
    /// - If you want to handle parsing error, please consider to use
    ///   [`ExifIter`].
    ///
    /// - If you have any custom defined tag which does not exist in
    ///   [`ExifTag`], you can always get the entry value by a raw tag code,
    ///   see [`Self::get_by_code`].
    ///
    ///   ## Example
    ///
    ///   ```rust
    ///   use nom_exif::*;
    ///
    ///   fn main() -> Result<()> {
    ///       let mut parser = MediaParser::new();
    ///       
    ///       let ms = MediaSource::open("./testdata/exif.jpg")?;
    ///       assert_eq!(ms.kind(), MediaKind::Image);
    ///       let iter = parser.parse_exif(ms)?;
    ///       let exif: Exif = iter.into();
    ///
    ///       assert_eq!(exif.get(ExifTag::Model).unwrap(), &"vivo X90 Pro+".into());
    ///       Ok(())
    ///   }
    pub fn get(&self, tag: ExifTag) -> Option<&EntryValue> {
        self.get_in(IfdIndex::MAIN, tag)
    }

    /// Get entry value for the specified `tag` in the specified `ifd`.
    ///
    /// *Note*:
    ///
    /// - The parsing error related to this tag won't be reported by this
    ///   method. Either this entry is not parsed successfully, or the tag does
    ///   not exist in the input data, this method will return None. Use
    ///   [`Self::errors`] to inspect per-entry errors.
    ///
    /// - For raw tag codes (e.g. unrecognized tags), use [`Self::get_by_code`].
    ///
    ///   ## Example
    ///
    ///   ```rust
    ///   use nom_exif::*;
    ///
    ///   fn main() -> Result<()> {
    ///       let mut parser = MediaParser::new();
    ///       let ms = MediaSource::open("./testdata/exif.jpg")?;
    ///       let iter = parser.parse_exif(ms)?;
    ///       let exif: Exif = iter.into();
    ///
    ///       assert_eq!(exif.get_in(IfdIndex::MAIN, ExifTag::Model).unwrap(),
    ///                  &"vivo X90 Pro+".into());
    ///       Ok(())
    ///   }
    ///   ```
    pub fn get_in(&self, ifd: IfdIndex, tag: ExifTag) -> Option<&EntryValue> {
        match tag.namespace() {
            // Contested code: look only where this tag is actually defined,
            // otherwise an absent tag would pick up its namesake's value.
            Some(kind) => self.get_by_code_in(ifd, kind, tag.code()),
            None => self.get_by_code(ifd, tag.code()),
        }
    }

    /// Get entry value for the specified raw `code` in the specified `ifd`.
    /// Used for tags not in the recognized [`ExifTag`] enum.
    ///
    /// A bare code cannot say *which* IFD namespace it belongs to. This scans
    /// `Tiff`, `Exif`, `Gps`, `Interop` in that order and returns the first
    /// hit; use [`Self::get_by_code_in`] when the namespace is known.
    pub fn get_by_code(&self, ifd: IfdIndex, code: u16) -> Option<&EntryValue> {
        self.ifds.get(ifd.as_usize()).and_then(|d| d.get_any(code))
    }

    /// Get entry value for a raw `code` within a specific IFD namespace.
    ///
    /// Unlike [`Self::get_by_code`] this is unambiguous — `(MAIN, Gps,
    /// 0x000b)` is `GPSDOP` and cannot return IFD0's `ProcessingSoftware`.
    pub fn get_by_code_in(&self, ifd: IfdIndex, kind: IfdKind, code: u16) -> Option<&EntryValue> {
        self.ifds
            .get(ifd.as_usize())
            .and_then(|d| d.get(kind, code))
    }

    /// Iterate every parsed entry in every IFD, carrying the IFD *namespace*
    /// each entry came from.
    ///
    /// Order is: IFD0 entries first (in `HashMap` order — not stable), then
    /// IFD1, etc. Filter with
    /// `.entries().filter(|e| e.ifd_kind() == IfdKind::Gps)`.
    ///
    /// Prefer this over [`Self::iter`], which cannot distinguish a GPS tag
    /// from an IFD0 tag that shares its code.
    pub fn entries(&self) -> impl Iterator<Item = ExifEntryRef<'_>> {
        self.ifds.iter().enumerate().flat_map(|(idx, dir)| {
            let ifd = IfdIndex::new(idx);
            dir.iter().map(move |(kind, code, value)| {
                ExifEntryRef::new(ifd, kind, TagOrCode::from_code_in(kind, code), value)
            })
        })
    }

    /// Iterate every parsed entry in every IFD.
    ///
    /// Order is: IFD0 entries first (in `HashMap` order — not stable), then
    /// IFD1, etc. Filter by IFD with `.iter().filter(|e| e.ifd == IfdIndex::MAIN)`.
    #[deprecated(
        since = "3.8.0",
        note = "use `entries()`: `ExifEntry` has no room for the IFD namespace, so tags that share a code across IFDs (e.g. 0x000b = ProcessingSoftware in IFD0, GPSDOP in the GPS IFD) are indistinguishable here"
    )]
    pub fn iter(&self) -> impl Iterator<Item = ExifEntry<'_>> {
        self.ifds.iter().enumerate().flat_map(|(idx, dir)| {
            let ifd = IfdIndex::new(idx);
            dir.iter().map(move |(kind, code, value)| ExifEntry {
                ifd,
                tag: TagOrCode::from_code_in(kind, code),
                value,
            })
        })
    }

    /// Get parsed GPS information.
    ///
    /// Returns `None` if the source had no `GPSInfo` IFD or if its parse
    /// failed (failures land in [`Self::errors`]).
    pub fn gps_info(&self) -> Option<&GPSInfo> {
        self.gps_info.as_ref()
    }

    /// Per-entry errors collected during `From<ExifIter>` conversion. Each
    /// tuple is `(ifd, tag, error)`. Empty slice if the parse was clean.
    pub fn errors(&self) -> &[(IfdIndex, TagOrCode, crate::EntryError)] {
        &self.errors
    }

    /// Whether the source file is known to embed a paired media track
    /// that this parse path did *not* surface — a Pixel/Google or Samsung
    /// Galaxy Motion Photo (JPEG with `GCamera:MotionPhoto` XMP and an
    /// MP4 trailer). Use [`crate::MediaParser::parse_track`] on the same
    /// source to extract the embedded track.
    ///
    /// **Content-detected, not MIME-guessed**: returns `true` only when
    /// `parse_exif` observed a concrete content signal
    /// (`GCamera:MotionPhoto="1"` plus a `Container:Directory` /
    /// `MotionPhotoOffset` / `MicroVideoOffset`). A plain JPEG or HEIC
    /// without such signals returns `false`.
    ///
    /// **Coverage**: Pixel/Google Motion Photos and Samsung Galaxy
    /// Motion Photos that use the Adobe XMP Container directory format
    /// (JPEG variants).
    pub fn has_embedded_track(&self) -> bool {
        self.has_embedded_track
    }

    /// Deprecated alias for [`Self::has_embedded_track`].
    #[deprecated(
        since = "3.1.0",
        note = "renamed to `has_embedded_track` to reflect the actual semantics (paired track hint, not arbitrary embedded media)"
    )]
    pub fn has_embedded_media(&self) -> bool {
        self.has_embedded_track()
    }

    fn put_value(&mut self, ifd: usize, kind: IfdKind, code: u16, v: EntryValue) {
        while self.ifds.len() < ifd + 1 {
            self.ifds.push(ParsedImageFileDirectory::new());
        }
        self.ifds[ifd].put(kind, code, v);
    }
}

impl From<ExifIter> for Exif {
    fn from(iter: ExifIter) -> Self {
        let gps_info = iter.parse_gps().ok().flatten();
        let has_embedded_track = iter.has_embedded_track();
        let mut exif = Exif::new(gps_info, has_embedded_track);

        for entry in iter {
            let ifd = entry.ifd();
            let kind = entry.ifd_kind();
            let tag = entry.tag();
            let code = tag.code();
            match entry.into_result() {
                Ok(v) => exif.put_value(ifd.as_usize(), kind, code, v),
                Err(e) => exif.errors.push((ifd, tag, e)),
            }
        }

        exif
    }
}

pub(crate) const TIFF_HEADER_LEN: usize = 8;

/// TIFF Header
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TiffHeader {
    pub endian: Endianness,
    pub ifd0_offset: u32,
}

impl Default for TiffHeader {
    fn default() -> Self {
        Self {
            endian: Endianness::Big,
            ifd0_offset: 0,
        }
    }
}

impl Debug for TiffHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let endian_str = match self.endian {
            Endianness::Big => "Big",
            Endianness::Little => "Little",
            Endianness::Native => "Native",
        };
        f.debug_struct("TiffHeader")
            .field("endian", &endian_str)
            .field("ifd0_offset", &format!("{:#x}", self.ifd0_offset))
            .finish()
    }
}

pub(crate) const IFD_ENTRY_SIZE: usize = 12;

impl TiffHeader {
    pub fn parse(input: &[u8]) -> IResult<&[u8], TiffHeader> {
        use nom::number::streaming::{u16, u32};
        let (remain, endian) = TiffHeader::parse_endian(input)?;
        let (_, (_, offset)) = (
            combinator::verify(u16(endian), |magic| *magic == 0x2a),
            u32(endian),
        )
            .parse(remain)?;

        let header = Self {
            endian,
            ifd0_offset: offset,
        };

        Ok((remain, header))
    }

    pub fn parse_ifd_entry_num(input: &[u8], endian: Endianness) -> IResult<&[u8], u16> {
        let (remain, num) = nom::number::streaming::u16(endian)(input)?; // Safe-slice
        if num == 0 {
            return Ok((remain, 0));
        }

        // 12 bytes per entry
        let size = (num as usize)
            .checked_mul(IFD_ENTRY_SIZE)
            .expect("should fit");

        if size > remain.len() {
            return Err(nom::Err::Incomplete(Needed::new(size - remain.len())));
        }

        Ok((remain, num))
    }

    // pub fn first_ifd<'a>(&self, input: &'a [u8], tag_ids: HashSet<u16>) -> IResult<&'a [u8], IFD> {
    //     // ifd0_offset starts from the beginning of Header, so we should
    //     // subtract the header size, which is 8
    //     let offset = self.ifd0_offset - 8;

    //     // skip to offset
    //     let (_, remain) = take(offset)(input)?;

    //     IFD::parse(remain, self.endian, tag_ids)
    // }

    fn parse_endian(input: &[u8]) -> IResult<&[u8], Endianness> {
        combinator::map(alt((tag("MM"), tag("II"))), |endian_marker| {
            if endian_marker == b"MM" {
                Endianness::Big
            } else {
                Endianness::Little
            }
        })
        .parse(input)
    }
}

pub(crate) fn check_exif_header(data: &[u8]) -> Result<bool, nom::Err<nom::error::Error<&[u8]>>> {
    tag::<_, _, nom::error::Error<_>>(EXIF_IDENT)(data).map(|_| true)
}

pub(crate) fn check_exif_header2(i: &[u8]) -> IResult<&[u8], ()> {
    let (remain, _) = (
        nom::number::complete::be_u32,
        nom::bytes::complete::tag(EXIF_IDENT),
    )
        .parse(i)?;
    Ok((remain, ()))
}

pub(crate) const EXIF_IDENT: &str = "Exif\0\0";
