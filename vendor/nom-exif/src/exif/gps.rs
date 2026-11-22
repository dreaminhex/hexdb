use std::str::FromStr;

use iso6709parse::ISO6709Coord;

use crate::values::{IRational, URational};

/// Parsed GPS information from the GPSInfo subIFD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GPSInfo {
    pub latitude_ref: LatRef,
    pub latitude: LatLng,
    pub longitude_ref: LonRef,
    pub longitude: LatLng,
    pub altitude: Altitude,
    pub speed: Option<Speed>,
}

impl Default for GPSInfo {
    fn default() -> Self {
        Self {
            latitude_ref: LatRef::North,
            latitude: LatLng::default(),
            longitude_ref: LonRef::East,
            longitude: LatLng::default(),
            altitude: Altitude::Unknown,
            speed: None,
        }
    }
}

/// Latitude or longitude expressed as degrees / minutes / seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatLng {
    pub degrees: URational,
    pub minutes: URational,
    pub seconds: URational,
}

/// Latitude hemisphere reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatRef {
    North,
    South,
}

impl LatRef {
    /// Construct from the 'N' / 'S' character carried in EXIF GPSLatitudeRef.
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            'N' | 'n' => Some(Self::North),
            'S' | 's' => Some(Self::South),
            _ => None,
        }
    }

    pub fn as_char(self) -> char {
        match self {
            Self::North => 'N',
            Self::South => 'S',
        }
    }

    /// +1.0 or -1.0 — useful when assembling decimal-degrees latitude.
    pub fn sign(self) -> f64 {
        match self {
            Self::North => 1.0,
            Self::South => -1.0,
        }
    }
}

/// Longitude hemisphere reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LonRef {
    East,
    West,
}

impl LonRef {
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            'E' | 'e' => Some(Self::East),
            'W' | 'w' => Some(Self::West),
            _ => None,
        }
    }

    pub fn as_char(self) -> char {
        match self {
            Self::East => 'E',
            Self::West => 'W',
        }
    }

    pub fn sign(self) -> f64 {
        match self {
            Self::East => 1.0,
            Self::West => -1.0,
        }
    }
}

/// Altitude relative to sea level.
///
/// Combines EXIF's `GPSAltitudeRef` (0 = above, 1 = below) with the magnitude
/// from `GPSAltitude` so the two cannot drift out of sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Altitude {
    /// Absent or unparseable.
    #[default]
    Unknown,
    AboveSeaLevel(URational),
    BelowSeaLevel(URational),
}

impl Altitude {
    /// Signed altitude in meters; `None` when Unknown or denominator=0.
    pub fn meters(&self) -> Option<f64> {
        match self {
            Altitude::Unknown => None,
            Altitude::AboveSeaLevel(r) => r.to_f64(),
            Altitude::BelowSeaLevel(r) => r.to_f64().map(|m| -m),
        }
    }

    /// The underlying magnitude rational, regardless of sign. None for `Unknown`.
    pub fn magnitude(&self) -> Option<URational> {
        match self {
            Altitude::Unknown => None,
            Altitude::AboveSeaLevel(r) | Altitude::BelowSeaLevel(r) => Some(*r),
        }
    }
}

/// EXIF GPS speed reference unit (`GPSSpeedRef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedUnit {
    KmPerHour,
    MilesPerHour,
    Knots,
}

impl SpeedUnit {
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            'K' | 'k' => Some(Self::KmPerHour),
            'M' | 'm' => Some(Self::MilesPerHour),
            'N' | 'n' => Some(Self::Knots),
            _ => None,
        }
    }

    pub fn as_char(self) -> char {
        match self {
            Self::KmPerHour => 'K',
            Self::MilesPerHour => 'M',
            Self::Knots => 'N',
        }
    }
}

/// EXIF GPS speed: unit + value paired so they cannot drift out of sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Speed {
    pub unit: SpeedUnit,
    pub value: URational,
}

impl LatLng {
    pub const fn new(degrees: URational, minutes: URational, seconds: URational) -> Self {
        Self {
            degrees,
            minutes,
            seconds,
        }
    }

    /// Convert to decimal degrees. Returns `None` if any component has a zero
    /// denominator.
    pub fn to_decimal_degrees(&self) -> Option<f64> {
        let d = self.degrees.to_f64()?;
        let m = self.minutes.to_f64()?;
        let s = self.seconds.to_f64()?;
        Some(d + m / 60.0 + s / 3600.0)
    }

    /// Construct from decimal degrees. Rejects NaN / ±inf and values whose
    /// magnitude exceeds 180° with `ConvertError::InvalidDecimalDegrees`.
    pub fn try_from_decimal_degrees(degrees: f64) -> Result<Self, crate::ConvertError> {
        if !degrees.is_finite() || degrees.abs() > 180.0 {
            return Err(crate::ConvertError::InvalidDecimalDegrees(degrees));
        }
        let abs = degrees.abs();
        let d = abs.trunc() as u32;
        let mins_total = (abs - d as f64) * 60.0;
        let m = mins_total.trunc() as u32;
        let secs_hundredths = ((mins_total - m as f64) * 60.0 * 100.0).round() as u32;
        Ok(Self::new(
            URational::new(d, 1),
            URational::new(m, 1),
            URational::new(secs_hundredths, 100),
        ))
    }
}

impl GPSInfo {
    /// Latitude in decimal degrees, signed by `latitude_ref` (positive = north).
    pub fn latitude_decimal(&self) -> Option<f64> {
        Some(self.latitude.to_decimal_degrees()? * self.latitude_ref.sign())
    }

    /// Longitude in decimal degrees, signed by `longitude_ref` (positive = east).
    pub fn longitude_decimal(&self) -> Option<f64> {
        Some(self.longitude.to_decimal_degrees()? * self.longitude_ref.sign())
    }

    /// Signed altitude in meters; `None` if altitude is `Unknown` or denominator=0.
    pub fn altitude_meters(&self) -> Option<f64> {
        self.altitude.meters()
    }

    /// Returns an ISO 6709 geographic point location string such as
    /// `+48.8577+002.295/`.
    pub fn to_iso6709(&self) -> String {
        let latitude = self.latitude.to_decimal_degrees().unwrap_or(0.0);
        let longitude = self.longitude.to_decimal_degrees().unwrap_or(0.0);
        let altitude_meters = self.altitude.meters();
        format!(
            "{}{latitude:08.5}{}{longitude:09.5}{}/",
            match self.latitude_ref {
                LatRef::North => '+',
                LatRef::South => '-',
            },
            match self.longitude_ref {
                LonRef::East => '+',
                LonRef::West => '-',
            },
            match altitude_meters {
                None | Some(0.0) => String::new(),
                Some(m) => format!(
                    "{}{}CRSWGS_84",
                    if m >= 0.0 { "+" } else { "-" },
                    Self::format_float(m.abs())
                ),
            }
        )
    }

    fn format_float(f: f64) -> String {
        if f.fract() == 0.0 {
            f.to_string()
        } else {
            format!("{f:.3}")
        }
    }
}

impl TryFrom<&[URational]> for LatLng {
    type Error = crate::Error;
    fn try_from(value: &[URational]) -> Result<Self, Self::Error> {
        if value.len() < 3 {
            return Err(crate::Error::Malformed {
                kind: crate::error::MalformedKind::IfdEntry,
                message: "need at least 3 URational components for LatLng".into(),
            });
        }
        Ok(Self {
            degrees: value[0],
            minutes: value[1],
            seconds: value[2],
        })
    }
}

impl TryFrom<&[IRational]> for LatLng {
    type Error = crate::Error;
    fn try_from(value: &[IRational]) -> Result<Self, Self::Error> {
        if value.len() < 3 {
            return Err(crate::Error::Malformed {
                kind: crate::error::MalformedKind::IfdEntry,
                message: "need at least 3 IRational components for LatLng".into(),
            });
        }
        let map_negative = |_| crate::Error::Malformed {
            kind: crate::error::MalformedKind::IfdEntry,
            message: "negative LatLng component".into(),
        };
        Ok(Self {
            degrees: URational::try_from(value[0]).map_err(map_negative)?,
            minutes: URational::try_from(value[1]).map_err(map_negative)?,
            seconds: URational::try_from(value[2]).map_err(map_negative)?,
        })
    }
}

impl TryFrom<&Vec<URational>> for LatLng {
    type Error = crate::Error;
    fn try_from(value: &Vec<URational>) -> Result<Self, Self::Error> {
        Self::try_from(value.as_slice())
    }
}

impl TryFrom<&Vec<IRational>> for LatLng {
    type Error = crate::Error;
    fn try_from(value: &Vec<IRational>) -> Result<Self, Self::Error> {
        Self::try_from(value.as_slice())
    }
}

impl FromStr for GPSInfo {
    type Err = crate::ConvertError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        iso6709parse::parse::<ISO6709Coord>(s)
            .map(|mut coord| {
                // `iso6709parse` only decodes the altitude field when a `CRS`
                // tag follows it, so Apple's default `±lat±lon±alt/` form (no
                // CRS) loses the altitude. Recover it ourselves. See #66.
                if coord.altitude.is_none() {
                    coord.altitude = parse_iso6709_altitude(s);
                }
                GPSInfo::from_iso6709_coord(coord)
            })
            .map_err(|_| crate::ConvertError::InvalidIso6709(s.to_string()))
    }
}

/// Extract the altitude (third signed numeric field) from an ISO 6709 string
/// representation such as `+47.7199-117.4931+522.171/`. Each field is a `+`/`-`
/// sign followed by a run of digits and dots, so a trailing `CRS…` suffix and
/// the closing `/` are skipped naturally. Returns `None` when there is no third
/// field (a plain `±lat±lon/` pair), so a two-field string never mistakes the
/// longitude for an altitude.
fn parse_iso6709_altitude(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut fields = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' || bytes[i] == b'-' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            fields += 1;
            if fields == 3 {
                return s[start..i].parse::<f64>().ok();
            }
        } else {
            i += 1;
        }
    }
    None
}

impl GPSInfo {
    /// Build a `GPSInfo` from a parsed ISO 6709 coordinate. Crate-internal:
    /// the public path is [`GPSInfo::from_str`] / `<GPSInfo as FromStr>::from_str`,
    /// which keeps `iso6709parse::ISO6709Coord` out of the public API surface
    /// (so an `iso6709parse` major-version bump does not force one here).
    pub(crate) fn from_iso6709_coord(v: ISO6709Coord) -> Self {
        let latitude_ref = if v.lat >= 0.0 {
            LatRef::North
        } else {
            LatRef::South
        };
        let longitude_ref = if v.lon >= 0.0 {
            LonRef::East
        } else {
            LonRef::West
        };
        let latitude = LatLng::try_from_decimal_degrees(v.lat.abs()).unwrap_or_default();
        let longitude = LatLng::try_from_decimal_degrees(v.lon.abs()).unwrap_or_default();
        let altitude = match v.altitude {
            None => Altitude::Unknown,
            Some(x) => {
                let mag = URational::new((x.abs() * 1000.0).trunc() as u32, 1000);
                if x >= 0.0 {
                    Altitude::AboveSeaLevel(mag)
                } else {
                    Altitude::BelowSeaLevel(mag)
                }
            }
        };
        Self {
            latitude_ref,
            latitude,
            longitude_ref,
            longitude,
            altitude,
            speed: None,
        }
    }
}
