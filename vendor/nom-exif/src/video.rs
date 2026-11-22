use std::collections::BTreeMap;

use crate::{
    ebml::webm::parse_webm,
    error::ParsingError,
    file::MediaMimeTrack,
    mov::{extract_moov_body_from_buf, parse_isobmff},
    EntryValue, GPSInfo,
};

/// Try to keep the tag name consistent with [`crate::ExifTag`], and add some
/// unique to video/audio, such as `DurationMs`.
///
/// Different variants of `TrackInfoTag` may have different value types, please
/// refer to the documentation of each variant.
#[derive(Debug, Clone, PartialEq, Eq, Copy, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TrackInfoTag {
    /// Its value is an `EntryValue::Text`.
    Make,

    /// Its value is an `EntryValue::Text`.
    Model,

    /// Its value is an `EntryValue::Text`.
    Software,

    /// Its value is an [`EntryValue::DateTime`].
    CreateDate,

    /// Duration in millisecond, its value is an `EntryValue::U64`.
    DurationMs,

    /// Its value is an `EntryValue::U32`.
    Width,

    /// Its value is an `EntryValue::U32`.
    Height,

    /// Its value is an `EntryValue::Text`, location presented in ISO6709.
    ///
    /// If you need a parsed [`GPSInfo`] which provides more detailed GPS info,
    /// please use [`TrackInfo::gps_info`].
    GpsIso6709,

    /// Its value is an `EntryValue::Text`.
    Author,
}

/// Represents parsed track info.
#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    entries: BTreeMap<TrackInfoTag, EntryValue>,
    gps_info: Option<GPSInfo>,
}

impl TrackInfo {
    /// Get value for `tag`. Different variants of `TrackInfoTag` may have
    /// different value types, please refer to [`TrackInfoTag`].
    pub fn get(&self, tag: TrackInfoTag) -> Option<&EntryValue> {
        self.entries.get(&tag)
    }

    /// Parsed GPS info, if `GpsIso6709` was present in the source. Mirrors
    /// [`Exif::gps_info`](crate::Exif::gps_info).
    pub fn gps_info(&self) -> Option<&GPSInfo> {
        self.gps_info.as_ref()
    }

    /// Iterate over `(tag, value)` pairs. The tag is yielded by value
    /// because [`TrackInfoTag`] is `Copy`. The parsed `GPSInfo` is **not**
    /// included here — get it via [`TrackInfo::gps_info`].
    pub fn iter(&self) -> impl Iterator<Item = (TrackInfoTag, &EntryValue)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// Deprecated: 3.0.0 reserved this for "track source also embeds a
    /// secondary track" cases (e.g. `.mka` audio container that also
    /// carries video) but the detection was never wired up — the method
    /// always returned `false`. v3.1 drops the symmetric counterpart on
    /// the image side ([`Exif::has_embedded_track`](crate::Exif::has_embedded_track))
    /// to a content-detected flag, but the track-source variant stays
    /// without a real use case, so this remains a no-op for source
    /// compatibility only. May be re-introduced if a concrete use case
    /// emerges.
    #[deprecated(
        since = "3.1.0",
        note = "no concrete use case in v3.x; always returned false in 3.0.0. Kept as a no-op for source-compat; will be removed if no use case emerges by v4."
    )]
    pub fn has_embedded_media(&self) -> bool {
        false
    }

    pub(crate) fn put(&mut self, tag: TrackInfoTag, value: EntryValue) {
        self.entries.insert(tag, value);
    }
}

/// Parse video/audio info from `reader`. The file format will be detected
/// automatically by parser, if the format is not supported, an `Err` will be
/// returned.
///
/// Currently supported file formats are:
///
/// - ISO base media file format (ISOBMFF): *.mp4, *.mov, *.3gp, etc.
/// - Matroska based file format: *.webm, *.mkv, *.mka, etc.
///
/// ## Explanation of the generic parameters of this function:
///
/// - In order to improve parsing efficiency, the parser will internally skip
///   some useless bytes during parsing the byte stream, which is called
///   `Skip` internally.
///
/// - In order to support both `Read` and `Read` + `Seek` types, the interface
///   of input parameters is defined as `Read`.
///   
/// - Since Rust does not support specialization, the parser cannot internally
///   distinguish between `Read` and `Seek` and provide different `Skip`
///   implementations for them.
///   
/// Therefore, We chose to let the user specify how `Skip` works:
///
/// - `parse_track_info::<SkipSeek, _>(reader)` means the `reader` supports
///   `Seek`, so `Skip` will use the `Seek` trait to implement efficient skip
///   operations.
///   
/// - `parse_track_info::<SkipRead, _>(reader)` means the `reader` dosn't
///   support `Seek`, so `Skip` will fall back to using `Read` to implement the
///   skip operations.
///
/// ## Performance impact
///
/// If your `reader` only supports `Read`, it may cause performance loss when
/// processing certain large files. For example, *.mov files place metadata at
/// the end of the file, therefore, when parsing such files, locating metadata
/// will be slightly slower.
///
/// ## Examples
///
/// ```rust
/// use nom_exif::*;
/// use std::fs::File;
///
/// let ms = MediaSource::open("./testdata/meta.mov").unwrap();
/// assert_eq!(ms.kind(), MediaKind::Track);
/// let mut parser = MediaParser::new();
/// let info: TrackInfo = parser.parse_track(ms).unwrap();
///
/// assert_eq!(info.get(TrackInfoTag::Make), Some(&"Apple".into()));
/// assert_eq!(info.get(TrackInfoTag::Model), Some(&"iPhone X".into()));
/// assert_eq!(info.get(TrackInfoTag::GpsIso6709), Some(&"+27.1281+100.2508+000.000/".into()));
/// assert_eq!(info.gps_info().unwrap().latitude_ref, LatRef::North);
/// assert_eq!(
///     info.gps_info().unwrap().latitude,
///     LatLng::new(URational::new(27, 1), URational::new(7, 1), URational::new(4116, 100)),
/// );
/// ```
#[tracing::instrument(skip(input))]
pub(crate) fn parse_track_info(
    input: &[u8],
    mime_video: MediaMimeTrack,
) -> Result<TrackInfo, ParsingError> {
    let mut info: TrackInfo = match mime_video {
        crate::file::MediaMimeTrack::QuickTime
        | crate::file::MediaMimeTrack::_3gpp
        | crate::file::MediaMimeTrack::Mp4 => {
            let range = extract_moov_body_from_buf(input)?;
            let moov_body = &input[range];
            parse_isobmff(moov_body)?
        }
        crate::file::MediaMimeTrack::Webm | crate::file::MediaMimeTrack::Matroska => {
            parse_webm(input)?.into()
        }
    };

    if let Some(gps) = info.get(TrackInfoTag::GpsIso6709) {
        info.gps_info = gps.as_str().and_then(|s| s.parse().ok());
    }

    Ok(info)
}

impl TrackInfoTag {
    /// Stable, programmatic name of this tag (matches the `Display` output).
    pub const fn name(self) -> &'static str {
        match self {
            TrackInfoTag::Make => "Make",
            TrackInfoTag::Model => "Model",
            TrackInfoTag::Software => "Software",
            TrackInfoTag::CreateDate => "CreateDate",
            TrackInfoTag::DurationMs => "DurationMs",
            TrackInfoTag::Width => "Width",
            TrackInfoTag::Height => "Height",
            TrackInfoTag::GpsIso6709 => "GpsIso6709",
            TrackInfoTag::Author => "Author",
        }
    }
}

impl std::fmt::Display for TrackInfoTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for TrackInfoTag {
    type Err = crate::ConvertError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Make" => TrackInfoTag::Make,
            "Model" => TrackInfoTag::Model,
            "Software" => TrackInfoTag::Software,
            "CreateDate" => TrackInfoTag::CreateDate,
            "DurationMs" => TrackInfoTag::DurationMs,
            "Width" => TrackInfoTag::Width,
            "Height" => TrackInfoTag::Height,
            "GpsIso6709" => TrackInfoTag::GpsIso6709,
            "Author" => TrackInfoTag::Author,
            other => return Err(crate::ConvertError::UnknownTagName(other.to_owned())),
        })
    }
}
