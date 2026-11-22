use std::{
    cmp::{max, min},
    fmt::{Debug, Display},
    fs::File,
    io::{self, Read, Seek},
    path::Path,
};

use crate::{
    error::{ParsedError, ParsingError, ParsingErrorState},
    exif::TiffHeader,
    file::MediaMime,
    ExifIter, TrackInfo,
};

/// A function that tries to skip `n` bytes of `reader` by seeking. Returns
/// `Ok(true)` on success, `Ok(false)` if the reader does not support seek
/// (so the caller should fall back to reading-and-discarding), or
/// `Err(io::Error)` if seek itself failed (e.g. truncated file handle).
///
/// This is captured at construction time by `MediaSource::seekable` /
/// `unseekable`, replacing the v2 `S: Skip<R>` phantom parameter with a
/// runtime fn pointer.
pub(crate) type SkipBySeekFn<R> = fn(&mut R, u64) -> io::Result<bool>;

/// `MediaSource` represents a media data source that can be parsed by
/// [`MediaParser`].
///
/// - Use [`MediaSource::open`] to create a MediaSource from a file path.
///
/// - Use [`MediaSource::from_bytes`] for zero-copy in-memory input
///   (`Vec<u8>`, `&'static [u8]`, [`bytes::Bytes`], …). Pair with
///   [`MediaParser::parse_exif_from_bytes`] / [`MediaParser::parse_track_from_bytes`].
///
/// - In other cases:
///
///   - Use [`MediaSource::seekable`] to create a MediaSource from a `Read + Seek`
///     (an already-open `File` goes here).
///
///   - Use [`MediaSource::unseekable`] to create a MediaSource from a
///     reader that only impl `Read`
///
/// *Note*: Please use [`MediaSource::seekable`] in preference to [`MediaSource::unseekable`],
/// since the former is more efficient when the parser needs to skip a large number of bytes.
///
/// Passing in a `BufRead` should be avoided because [`MediaParser`] comes with
/// its own buffer management and the buffers can be shared between multiple
/// parsing tasks, thus avoiding frequent memory allocations.
pub struct MediaSource<R> {
    pub(crate) reader: R,
    pub(crate) buf: Vec<u8>,
    pub(crate) mime: MediaMime,
    pub(crate) skip_by_seek: SkipBySeekFn<R>,
    /// P7: zero-copy memory-mode payload. `Some` only when the source was
    /// built via [`MediaSource::<()>::from_bytes`]; `reader`, `buf`, and
    /// `skip_by_seek` are placeholders (and never consulted) in that mode.
    pub(crate) memory: Option<bytes::Bytes>,
}

/// Top-level classification of a media source.
///
/// `Image` files carry EXIF metadata (parse with `MediaParser::parse_exif`);
/// `Track` files are time-based containers — video, audio, or both — and
/// carry track-info metadata (parse with `MediaParser::parse_track`). Pure
/// audio containers like `.mka` are classified as `Track`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Track,
}

impl<R> Debug for MediaSource<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSource")
            .field("mime", &self.mime)
            .finish_non_exhaustive()
    }
}

// Should be enough for parsing header
const HEADER_PARSE_BUF_SIZE: usize = 128;

impl<R> MediaSource<R> {
    /// Top-level classification of this media source.
    pub fn kind(&self) -> MediaKind {
        match self.mime {
            MediaMime::Image(_) => MediaKind::Image,
            MediaMime::Track(_) => MediaKind::Track,
        }
    }
}

impl<R: Read> MediaSource<R> {
    fn build(mut reader: R, skip_by_seek: SkipBySeekFn<R>) -> crate::Result<Self> {
        let mut buf = Vec::with_capacity(HEADER_PARSE_BUF_SIZE);
        reader
            .by_ref()
            .take(HEADER_PARSE_BUF_SIZE as u64)
            .read_to_end(&mut buf)?;
        let mime: MediaMime = buf.as_slice().try_into()?;
        Ok(Self {
            reader,
            buf,
            mime,
            skip_by_seek,
            memory: None,
        })
    }

    /// Use [`MediaSource::unseekable`] to create a MediaSource from a
    /// reader that only impl `Read`
    ///
    /// *Note*: Please use [`MediaSource::seekable`] in preference to [`MediaSource::unseekable`],
    /// since the former is more efficient when the parser needs to skip a large number of bytes.
    pub fn unseekable(reader: R) -> crate::Result<Self> {
        Self::build(reader, |_, _| Ok(false))
    }
}

impl<R: Read + Seek> MediaSource<R> {
    /// Use [`MediaSource::seekable`] to create a MediaSource from a `Read + Seek`
    ///
    /// *Note*: Please use [`MediaSource::seekable`] in preference to [`MediaSource::unseekable`],
    /// since the former is more efficient when the parser needs to skip a large number of bytes.
    pub fn seekable(reader: R) -> crate::Result<Self> {
        Self::build(reader, |r, n| {
            let signed: i64 = n
                .try_into()
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            r.seek_relative(signed)?;
            Ok(true)
        })
    }
}

impl MediaSource<File> {
    /// Open a file at `path` and parse its header to detect the media format.
    ///
    /// This is the v3-preferred entry point for the common case of "I have a
    /// path on disk". For an already-open `File` use [`Self::seekable`].
    pub fn open<P: AsRef<Path>>(path: P) -> crate::Result<Self> {
        Self::seekable(File::open(path)?)
    }
}

impl MediaSource<()> {
    /// Build a [`MediaSource`] from an in-memory byte payload.
    ///
    /// Accepts any type convertible into [`bytes::Bytes`] — `Bytes`,
    /// `Vec<u8>`, `&'static [u8]`, [`bytes::Bytes::from_owner`] outputs, and
    /// HTTP-stack body types that implement `Into<Bytes>` directly.
    ///
    /// The header (first up to 128 bytes) is sniffed for media kind, the
    /// same way [`MediaSource::open`] does it for files. The full payload is
    /// stored zero-copy: subsequent parsing through
    /// [`MediaParser::parse_exif_from_bytes`] / [`MediaParser::parse_track_from_bytes`]
    /// shares this `Bytes` directly with the returned `ExifIter` / sub-IFDs
    /// via reference counting.
    ///
    /// The returned source is parsed by the dedicated
    /// [`MediaParser::parse_exif_from_bytes`] / [`MediaParser::parse_track_from_bytes`]
    /// methods. The streaming `parse_exif` / `parse_track` methods do not
    /// accept `MediaSource<()>` (their `R: Read` bound is unsatisfiable).
    ///
    /// # Example
    ///
    /// ```rust
    /// use nom_exif::{MediaSource, MediaParser, MediaKind};
    ///
    /// let bytes = std::fs::read("./testdata/exif.jpg")?;
    /// let ms = MediaSource::from_bytes(bytes)?;
    /// assert_eq!(ms.kind(), MediaKind::Image);
    ///
    /// let mut parser = MediaParser::new();
    /// let _iter = parser.parse_exif_from_bytes(ms)?;
    /// # Ok::<(), nom_exif::Error>(())
    /// ```
    #[deprecated(
        since = "3.3.0",
        note = "Use `MediaSource::from_memory` and the unified `parse_*` \
                methods (which now accept memory-mode sources directly). \
                The `MediaSource<()>` shape will be removed in v4."
    )]
    pub fn from_bytes(bytes: impl Into<bytes::Bytes>) -> crate::Result<Self> {
        let bytes = bytes.into();
        let head_end = bytes.len().min(HEADER_PARSE_BUF_SIZE);
        let mime: MediaMime = bytes[..head_end].try_into()?;
        Ok(Self {
            reader: (),
            buf: Vec::new(),
            mime,
            // Placeholder: never invoked in memory mode (clear_and_skip's
            // AdvanceOnly path is the only one taken).
            skip_by_seek: |_, _| Ok(false),
            memory: Some(bytes),
        })
    }

    /// Internal adapter: convert a v3.0-style `MediaSource<()>` (built via
    /// the deprecated `from_bytes`) into the unified `MediaSource<Empty>`
    /// shape so the deprecated `parse_*_from_bytes` methods can delegate to
    /// the unified `parse_*` methods. Memory contents are moved over
    /// verbatim, preserving zero-copy.
    pub(crate) fn into_empty(self) -> MediaSource<std::io::Empty> {
        MediaSource {
            reader: std::io::empty(),
            buf: self.buf,
            mime: self.mime,
            // Placeholder: never invoked in memory mode (clear_and_skip's
            // AdvanceOnly path is the only one taken).
            skip_by_seek: |_, _| Ok(false),
            memory: self.memory,
        }
    }
}

impl MediaSource<std::io::Empty> {
    /// Build a [`MediaSource`] from an in-memory byte payload.
    ///
    /// This is the v3.3 replacement for [`MediaSource::<()>::from_bytes`]
    /// (which is now `#[deprecated]`). Functionally identical — same
    /// zero-copy semantics, same accepted input types — but produces a
    /// `MediaSource<std::io::Empty>` so that the unified `parse_*<R: Read>`
    /// methods can accept it directly without a separate `_from_bytes`
    /// sibling.
    ///
    /// Accepts any type convertible into [`bytes::Bytes`] — `Bytes`,
    /// `Vec<u8>`, `&'static [u8]`, `String`, `Box<[u8]>`, and HTTP-stack
    /// body types that implement `Into<Bytes>` directly.
    ///
    /// The header (first up to 128 bytes) is sniffed for media kind, the
    /// same way [`MediaSource::open`] does it for files. The full payload
    /// is stored zero-copy: subsequent parsing through
    /// [`MediaParser::parse_exif`] / [`MediaParser::parse_track`] shares
    /// this `Bytes` directly with the returned `ExifIter` / sub-IFDs via
    /// reference counting.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nom_exif::{MediaSource, MediaParser, MediaKind};
    ///
    /// let bytes = std::fs::read("./testdata/exif.jpg")?;
    /// let ms = MediaSource::from_memory(bytes)?;
    /// assert_eq!(ms.kind(), MediaKind::Image);
    ///
    /// let mut parser = MediaParser::new();
    /// let _iter = parser.parse_exif(ms)?;  // unified entry point
    /// # Ok::<(), nom_exif::Error>(())
    /// ```
    pub fn from_memory(bytes: impl Into<bytes::Bytes>) -> crate::Result<Self> {
        let bytes = bytes.into();
        let head_end = bytes.len().min(HEADER_PARSE_BUF_SIZE);
        let mime: MediaMime = bytes[..head_end].try_into()?;
        Ok(Self {
            reader: std::io::empty(),
            buf: Vec::new(),
            mime,
            // Placeholder: never invoked in memory mode (AdvanceOnly path).
            skip_by_seek: |_, _| Ok(false),
            memory: Some(bytes),
        })
    }
}

// ----- Parse-time buffer policy -----
//
// Layered by lifecycle:
//
// - `INIT_BUF_SIZE` — first fill into the parse loop and the initial
//   `Vec::with_capacity` for fresh allocations. Modest so cold one-shot
//   helpers don't over-commit.
// - `MIN_GROW_SIZE` — floor for every subsequent fill once we're in deep
//   parse. Larger than `INIT_BUF_SIZE` to amortize syscalls / async
//   blocking-pool dispatches.
// - `MAX_PARSE_BUF_SIZE` — hard cap on cumulative buffer growth during a
//   single parse. Anything that would push past this is rejected as
//   `io::ErrorKind::Unsupported`; defense against crafted box/IFD headers
//   that declare absurd sizes.
// - `MAX_REUSE_BUF_SIZE` — soft cap on the buffer kept between parses for
//   recycling. After a parse whose buffer ended above this, `shrink_to`
//   gives the excess back to the allocator. Tuned for typical metadata
//   sizes (HEIC Live Photo / large CR3 / IIQ all fit under 4 MB) so the
//   recycle path stays warm for batch workloads.
pub(crate) const INIT_BUF_SIZE: usize = 8 * 1024;
pub(crate) const MIN_GROW_SIZE: usize = 16 * 1024;
pub(crate) const MAX_PARSE_BUF_SIZE: usize = 1024 * 1024 * 1024;
const MAX_REUSE_BUF_SIZE: usize = 4 * 1024 * 1024;

pub(crate) trait Buf {
    fn buffer(&self) -> &[u8];
    fn clear(&mut self);

    fn set_position(&mut self, pos: usize);
    #[allow(unused)]
    fn position(&self) -> usize;
}

/// Buffer-management state used by `MediaParser` (sync and async paths share it).
///
/// Holds at most one *active* `Vec<u8>` (being filled by the current parse) and
/// one *cached* `Bytes` clone of the most recently shared buffer. When the
/// next parse starts, the cache is consulted: if `Bytes::try_into_mut`
/// succeeds the underlying allocation is reused (the previous `ExifIter`
/// has been dropped); otherwise the clone is discarded and a fresh
/// `Vec<u8>` is allocated.
///
/// This replaces the v2 multi-slot `Buffers` pool — `MediaParser` methods
/// are `&mut self`, so a single slot is sufficient.
#[derive(Debug, Default)]
pub(crate) struct BufferedParserState {
    cached: Option<bytes::Bytes>,
    buf: Option<Vec<u8>>,
    /// P7: memory-mode storage. When `Some`, the parser is feeding from a
    /// caller-owned `Bytes` instead of streaming via a reader. `buf` and
    /// `cached` are unused in this mode — the user owns the allocation,
    /// so there is nothing to recycle.
    memory: Option<bytes::Bytes>,
    position: usize,
}

impl BufferedParserState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self) {
        // If a parse failed mid-way the buf may still be present; drop it.
        // Cache stays — recycle on next acquire if eligible.
        self.buf = None;
        self.memory = None;
        self.position = 0;
    }

    /// Switch the parser state into memory mode, owning `bytes` directly.
    /// Caller must have already called `reset()` (asserted in debug). Subsequent
    /// `share_buf` returns a clone of `bytes` (zero-copy: `Bytes::clone` is a
    /// refcount bump). Subsequent `Buf::buffer()` returns `&bytes[position..]`.
    pub(crate) fn set_memory(&mut self, bytes: bytes::Bytes) {
        debug_assert!(
            self.buf.is_none() && self.memory.is_none(),
            "set_memory called on non-clean state"
        );
        self.memory = Some(bytes);
        self.position = 0;
    }

    pub(crate) fn is_memory_mode(&self) -> bool {
        self.memory.is_some()
    }

    pub(crate) fn acquire_buf(&mut self) {
        if self.memory.is_some() {
            // Memory mode: nothing to acquire — `buffer()` reads from `memory`.
            return;
        }
        debug_assert!(self.buf.is_none());
        let buf = match self.cached.take() {
            Some(b) => match b.try_into_mut() {
                Ok(bm) => {
                    let mut v = Vec::<u8>::from(bm);
                    v.clear();
                    if v.capacity() > MAX_REUSE_BUF_SIZE {
                        v.shrink_to(MAX_REUSE_BUF_SIZE);
                    }
                    v
                }
                Err(_still_shared) => Vec::with_capacity(INIT_BUF_SIZE),
            },
            None => Vec::with_capacity(INIT_BUF_SIZE),
        };
        self.buf = Some(buf);
    }

    pub(crate) fn buf(&self) -> &Vec<u8> {
        self.buf.as_ref().expect("no buf here")
    }

    pub(crate) fn buf_mut(&mut self) -> &mut Vec<u8> {
        self.buf.as_mut().expect("no buf here")
    }


}

impl Buf for BufferedParserState {
    fn buffer(&self) -> &[u8] {
        if let Some(m) = &self.memory {
            return &m[self.position..];
        }
        &self.buf()[self.position..]
    }
    fn clear(&mut self) {
        // In memory mode `clear` is a no-op: there is no scratch buffer to
        // truncate, and the caller's bytes must remain available for further
        // parse_loop_step iterations. clear_and_skip's AdvanceOnly path is
        // what advances `position` in memory mode.
        if self.memory.is_some() {
            return;
        }
        self.buf_mut().clear();
    }
    fn set_position(&mut self, pos: usize) {
        self.position = pos;
    }
    fn position(&self) -> usize {
        self.position
    }
}

impl ShareBuf for BufferedParserState {
    fn share_buf(&mut self) -> (bytes::Bytes, usize) {
        if let Some(m) = self.memory.take() {
            // Zero-copy share: caller already owns the allocation. No cache
            // write — recycle is irrelevant when the user holds the alloc.
            let position = self.position;
            return (m, position);
        }
        let vec = self.buf.take().expect("no buf to share");
        let bytes = bytes::Bytes::from(vec);
        let position = self.position;
        self.cached = Some(bytes.clone());
        (bytes, position)
    }
}

/// What `clear_and_skip` should do, given the current buffer state and
/// the requested skip count.
pub(crate) enum SkipPlan {
    /// Skip is fully within the current buffer; just advance position.
    AdvanceOnly,
    /// Buffer must be cleared and `extra` bytes skipped from the reader.
    ClearAndSkip { extra: usize },
}

pub(crate) fn clear_and_skip_decide(buffer_len: usize, n: usize) -> SkipPlan {
    if n <= buffer_len {
        SkipPlan::AdvanceOnly
    } else {
        SkipPlan::ClearAndSkip {
            extra: n - buffer_len,
        }
    }
}

pub(crate) fn check_fill_size(existing_len: usize, requested: usize) -> io::Result<()> {
    if requested.saturating_add(existing_len) > MAX_PARSE_BUF_SIZE {
        tracing::error!(?requested, "the requested buffer size is too big");
        return Err(io::ErrorKind::Unsupported.into());
    }
    Ok(())
}

pub(crate) enum LoopAction<O> {
    /// Parse succeeded; return this value to the caller.
    Done(O),
    /// Need more bytes — call `fill_buf(reader, n)` then re-step.
    NeedFill(usize),
    /// Need to skip bytes — call `clear_and_skip(reader, n)` then re-step.
    Skip(usize),
    /// Parse failed permanently. Carries the structural-unit kind so
    /// the eventual `Error::Malformed` is labelled correctly.
    Failed {
        kind: crate::error::MalformedKind,
        message: String,
    },
}

/// Closure type passed to [`parse_loop_step`].
pub(crate) type ParseFn<'a, O> =
    dyn FnMut(&[u8], usize, Option<ParsingState>) -> Result<O, ParsingErrorState> + 'a;

/// Drives one iteration of the parse-loop algorithm. Pure (no I/O).
pub(crate) fn parse_loop_step<O>(
    buffer: &[u8],
    offset: usize,
    parsing_state: &mut Option<ParsingState>,
    parse: &mut ParseFn<'_, O>,
) -> LoopAction<O> {
    match parse(buffer, offset, parsing_state.take()) {
        Ok(o) => LoopAction::Done(o),
        Err(es) => {
            *parsing_state = es.state;
            match es.err {
                ParsingError::Need(n) => LoopAction::NeedFill(n),
                ParsingError::ClearAndSkip(n) => LoopAction::Skip(n),
                ParsingError::Failed { kind, message } => LoopAction::Failed { kind, message },
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ParsingState {
    TiffHeader(TiffHeader),
    HeifExifSize(usize),
    Cr3ExifSize(usize),
    /// PNG chunk walker state carried across streaming skips.
    Png(crate::png::PngState),
    /// WebP RIFF walker has validated the 12-byte header and skipped some
    /// chunks. Carries the number of chunk-stream bytes still remaining
    /// (from the resumed buffer's start) so the walker can stop cleanly at
    /// the end of the RIFF chunk region — WebP has no terminator chunk.
    WebpPastHeader(usize),
}

impl Display for ParsingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParsingState::TiffHeader(h) => Display::fmt(&format!("ParsingState: {h:?})"), f),
            ParsingState::HeifExifSize(n) => Display::fmt(&format!("ParsingState: {n}"), f),
            ParsingState::Cr3ExifSize(n) => Display::fmt(&format!("ParsingState: {n}"), f),
            ParsingState::Png(_) => f.write_str("ParsingState: Png"),
            ParsingState::WebpPastHeader(n) => {
                Display::fmt(&format!("ParsingState: WebpPastHeader({n})"), f)
            }
        }
    }
}

// Modern replacement for the `Load` trait in loader.rs. Adds offset-aware
// parsing and `ParsingState` threading for format-specific state machines.
pub(crate) trait BufParser: Buf + Debug {
    fn fill_buf<R: Read>(&mut self, reader: &mut R, size: usize) -> io::Result<usize>;

    fn load_and_parse<R: Read, P, O>(
        &mut self,
        reader: &mut R,
        skip_by_seek: SkipBySeekFn<R>,
        mut parse: P,
    ) -> Result<O, ParsedError>
    where
        P: FnMut(&[u8], Option<ParsingState>) -> Result<O, ParsingErrorState>,
    {
        self.load_and_parse_with_offset(
            reader,
            skip_by_seek,
            |data, _, state| parse(data, state),
            0,
        )
    }

    #[tracing::instrument(skip_all)]
    fn load_and_parse_with_offset<R: Read, P, O>(
        &mut self,
        reader: &mut R,
        skip_by_seek: SkipBySeekFn<R>,
        mut parse: P,
        offset: usize,
    ) -> Result<O, ParsedError>
    where
        P: FnMut(&[u8], usize, Option<ParsingState>) -> Result<O, ParsingErrorState>,
    {
        if offset >= self.buffer().len() {
            self.fill_buf(reader, MIN_GROW_SIZE)?;
        }
        let mut parsing_state: Option<ParsingState> = None;
        loop {
            match parse_loop_step(self.buffer(), offset, &mut parsing_state, &mut parse) {
                LoopAction::Done(o) => return Ok(o),
                LoopAction::NeedFill(needed) => {
                    let to_read = max(needed, MIN_GROW_SIZE);
                    let n = self.fill_buf(reader, to_read)?;
                    if n == 0 {
                        return Err(ParsedError::NoEnoughBytes);
                    }
                }
                LoopAction::Skip(n) => {
                    self.clear_and_skip(reader, skip_by_seek, n)?;
                }
                LoopAction::Failed { kind, message } => {
                    return Err(ParsedError::Failed { kind, message })
                }
            }
        }
    }

    #[tracing::instrument(skip(reader, skip_by_seek))]
    fn clear_and_skip<R: Read>(
        &mut self,
        reader: &mut R,
        skip_by_seek: SkipBySeekFn<R>,
        n: usize,
    ) -> Result<(), ParsedError> {
        match clear_and_skip_decide(self.buffer().len(), n) {
            SkipPlan::AdvanceOnly => {
                self.set_position(self.position() + n);
                Ok(())
            }
            SkipPlan::ClearAndSkip { extra: skip_n } => {
                self.clear();
                let done = (skip_by_seek)(
                    reader,
                    skip_n.try_into().map_err(|_| ParsedError::Failed {
                        // No format context available here: the parser
                        // hit an internal limit honoring a caller's skip.
                        // Pick a sensible default — see #55 follow-up.
                        kind: crate::error::MalformedKind::IsoBmffBox,
                        message: "skip too many bytes".into(),
                    })?,
                )?;
                if !done {
                    let mut skipped = 0;
                    while skipped < skip_n {
                        let mut to_skip = skip_n - skipped;
                        to_skip = min(to_skip, MAX_PARSE_BUF_SIZE);
                        let n = self.fill_buf(reader, to_skip)?;
                        skipped += n;
                        if skipped <= skip_n {
                            self.clear();
                        } else {
                            let remain = skipped - skip_n;
                            self.set_position(self.buffer().len() - remain);
                            break;
                        }
                    }
                }

                if self.buffer().is_empty() {
                    self.fill_buf(reader, MIN_GROW_SIZE)?;
                }
                Ok(())
            }
        }
    }
}

impl BufParser for MediaParser {
    #[tracing::instrument(skip(self, reader), fields(buf_len=self.state.buffer().len()))]
    fn fill_buf<R: Read>(&mut self, reader: &mut R, size: usize) -> io::Result<usize> {
        if self.state.is_memory_mode() {
            // Memory mode owns every byte it will ever have. A request for
            // more is "the parser walked off the end of the input"; surface
            // it the same way the streaming path surfaces a 0-byte read.
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        check_fill_size(self.state.buf().len(), size)?;

        // Do not pre-allocate `size` bytes: a crafted box header can declare a
        // huge extended size (up to MAX_PARSE_BUF_SIZE) that far exceeds the actual
        // stream length. reserve_exact would allocate that memory immediately
        // even when the reader has only a few bytes left. read_to_end grows the
        // buffer from the reader's actual size hint instead.
        let n = reader.take(size as u64).read_to_end(self.state.buf_mut())?;
        if n == 0 {
            tracing::error!(buf_len = self.state.buf().len(), "fill_buf: EOF");
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }

        tracing::debug!(
            ?size,
            ?n,
            buf_len = self.state.buf().len(),
            "fill_buf: read bytes"
        );

        Ok(n)
    }
}

impl Buf for MediaParser {
    fn buffer(&self) -> &[u8] {
        self.state.buffer()
    }

    fn clear(&mut self) {
        self.state.clear();
    }

    fn set_position(&mut self, pos: usize) {
        self.state.set_position(pos);
    }

    fn position(&self) -> usize {
        self.state.position()
    }
}

/// A `MediaParser` can parse media info from a [`MediaSource`].
///
/// `MediaParser` manages inner parse buffers that can be shared between
/// multiple parsing tasks, thus avoiding frequent memory allocations.
///
/// Therefore:
///
/// - Try to reuse a `MediaParser` instead of creating a new one every time
///   you need it.
///
/// - `MediaSource` should be created directly from `Read`, not from `BufRead`.
///
/// ## Example
///
/// ```rust
/// use nom_exif::*;
///
/// let mut parser = MediaParser::new();
///
/// // ------------------- Parse Exif Info
/// let ms = MediaSource::open("./testdata/exif.heic").unwrap();
/// assert_eq!(ms.kind(), MediaKind::Image);
/// let mut iter = parser.parse_exif(ms).unwrap();
///
/// let entry = iter.next().unwrap();
/// assert!(matches!(entry.tag(), nom_exif::TagOrCode::Tag(ExifTag::Make)));
/// assert_eq!(entry.value().unwrap().as_str().unwrap(), "Apple");
///
/// // Convert `ExifIter` into an `Exif`. Clone it before converting, so that
/// // we can start the iteration from the beginning.
/// let exif: Exif = iter.clone().into();
/// assert_eq!(exif.get(ExifTag::Make).unwrap().as_str().unwrap(), "Apple");
///
/// // ------------------- Parse Track Info
/// let ms = MediaSource::open("./testdata/meta.mov").unwrap();
/// assert_eq!(ms.kind(), MediaKind::Track);
/// let info = parser.parse_track(ms).unwrap();
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
pub struct MediaParser {
    state: BufferedParserState,
}

impl Debug for MediaParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaParser")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Default for MediaParser {
    fn default() -> Self {
        Self {
            state: BufferedParserState::new(),
        }
    }
}

pub(crate) trait ShareBuf {
    /// Take ownership of the parser's active buffer and return the full
    /// allocation as `Bytes` plus the parser's `position` at share-time.
    /// Caller is responsible for slicing: a parse-loop range `r` corresponds
    /// to absolute range `(r.start + position)..(r.end + position)`.
    fn share_buf(&mut self) -> (bytes::Bytes, usize);
}

impl ShareBuf for MediaParser {
    fn share_buf(&mut self) -> (bytes::Bytes, usize) {
        self.state.share_buf()
    }
}

impl MediaParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse Exif metadata from an image source. Returns `Error::ExifNotFound`
    /// if the source is a `Track` (use [`Self::parse_track`] instead).
    ///
    /// As of v3.3, this method also accepts memory-mode sources built via
    /// [`MediaSource::from_memory`]. The deprecated [`Self::parse_exif_from_bytes`]
    /// is now a thin adapter that delegates here.
    ///
    /// `MediaParser` reuses its internal parse buffer across calls, so prefer
    /// reusing a single `MediaParser` over creating a new one per file. Drop
    /// the returned [`ExifIter`] (or convert it into [`crate::Exif`]) before
    /// the next `parse_*` call so the buffer can be reclaimed.
    pub fn parse_exif<R: Read>(&mut self, mut ms: MediaSource<R>) -> crate::Result<ExifIter> {
        self.reset();
        let res: crate::Result<ExifIter> = (|| {
            if let Some(memory) = ms.memory.take() {
                // Memory-mode: zero-copy share of caller-owned bytes.
                self.state.set_memory(memory);
                if !matches!(ms.mime, crate::file::MediaMime::Image(_)) {
                    return Err(crate::Error::ExifNotFound);
                }
                crate::exif::parse_exif_iter(
                    self,
                    ms.mime.unwrap_image(),
                    &mut ms.reader,
                    ms.skip_by_seek,
                )
            } else {
                // Streaming-mode: existing path verbatim.
                self.acquire_buf();
                self.buf_mut().append(&mut ms.buf);
                // PNG-only EOF tolerance: a tEXt-only PNG can be smaller
                // than HEADER_PARSE_BUF_SIZE (e.g. 117-byte text-only.png),
                // so the mime-detection prefill consumes the whole reader
                // and fill_buf returns UnexpectedEof. The bytes we need
                // are already in the parse buffer — proceed. Other formats
                // keep the strict-EOF contract.
                let is_png = matches!(
                    ms.mime,
                    crate::file::MediaMime::Image(
                        crate::file::MediaMimeImage::Png | crate::file::MediaMimeImage::Webp
                    )
                );
                match self.fill_buf(&mut ms.reader, INIT_BUF_SIZE) {
                    Ok(_) => {}
                    Err(e)
                        if is_png
                            && !self.buffer().is_empty()
                            && e.kind() == io::ErrorKind::UnexpectedEof => {}
                    Err(e) => return Err(e.into()),
                }
                if !matches!(ms.mime, crate::file::MediaMime::Image(_)) {
                    return Err(crate::Error::ExifNotFound);
                }
                crate::exif::parse_exif_iter(
                    self,
                    ms.mime.unwrap_image(),
                    &mut ms.reader,
                    ms.skip_by_seek,
                )
            }
        })();
        self.reset();
        res
    }

    /// Parse track info from a video/audio source.
    ///
    /// Parse track info from a video/audio source.
    ///
    /// In v3.1, this also accepts JPEG images that carry an embedded
    /// Pixel/Google Motion Photo trailer. As of v3.3, it also accepts
    /// memory-mode sources built via [`MediaSource::from_memory`]; the
    /// deprecated [`Self::parse_track_from_bytes`] is now a thin
    /// adapter that delegates here.
    pub fn parse_track<R: Read>(&mut self, mut ms: MediaSource<R>) -> crate::Result<TrackInfo> {
        self.reset();
        let res: crate::Result<TrackInfo> = (|| {
            if let Some(memory) = ms.memory.take() {
                // Memory mode: zero-copy.
                self.state.set_memory(memory);
                let mime_track = match ms.mime {
                    crate::file::MediaMime::Image(_) => return Err(crate::Error::TrackNotFound),
                    crate::file::MediaMime::Track(t) => t,
                };
                let out = self.load_and_parse(&mut ms.reader, ms.skip_by_seek, |data, _| {
                    crate::video::parse_track_info(data, mime_track)
                        .map_err(|e| ParsingErrorState::new(e, None))
                })?;
                Ok(out)
            } else {
                // Streaming mode: existing path verbatim.
                self.acquire_buf();
                self.buf_mut().append(&mut ms.buf);
                self.fill_buf(&mut ms.reader, INIT_BUF_SIZE)?;
                match ms.mime {
                    crate::file::MediaMime::Image(crate::file::MediaMimeImage::Jpeg) => {
                        self.parse_jpeg_motion_photo(&mut ms.reader)
                    }
                    crate::file::MediaMime::Image(_) => Err(crate::Error::TrackNotFound),
                    crate::file::MediaMime::Track(mime_track) => {
                        let skip = ms.skip_by_seek;
                        Ok(self.load_and_parse(ms.reader.by_ref(), skip, |data, _| {
                            crate::video::parse_track_info(data, mime_track)
                                .map_err(|e| ParsingErrorState::new(e, None))
                        })?)
                    }
                }
            }
        })();
        self.reset();
        res
    }

    /// Read a JPEG to EOF, locate a Pixel-style Motion Photo MP4 trailer,
    /// and parse it as track metadata. Returns
    /// [`crate::Error::TrackNotFound`] if no Motion Photo signal is
    /// present in the JPEG's XMP.
    fn parse_jpeg_motion_photo<R: Read>(&mut self, reader: &mut R) -> crate::Result<TrackInfo> {
        // Drain the rest of the JPEG into the parse buffer so we can
        // address the trailing MP4 by its byte offset from EOF.
        reader.read_to_end(self.buf_mut())?;
        let buf = self.buf_mut();
        let Some(offset) = crate::jpeg::find_motion_photo_offset(buf) else {
            return Err(crate::Error::TrackNotFound);
        };
        let trailer_start = (buf.len() as u64)
            .checked_sub(offset)
            .ok_or(crate::Error::TrackNotFound)? as usize;
        let trailer = &buf[trailer_start..];

        // The trailer can be MP4 / MOV / 3gp depending on the source device;
        // dispatch by sniffing it as a fresh ISO BMFF input.
        let trailer_mime =
            crate::file::MediaMime::try_from(trailer).map_err(|_| crate::Error::TrackNotFound)?;
        let mime_track = match trailer_mime {
            crate::file::MediaMime::Track(t) => t,
            crate::file::MediaMime::Image(_) => return Err(crate::Error::TrackNotFound),
        };
        crate::video::parse_track_info(trailer, mime_track).map_err(|e| match e {
            crate::error::ParsingError::Need(_) | crate::error::ParsingError::ClearAndSkip(_) => {
                crate::Error::UnexpectedEof {
                    context: "motion-photo trailer",
                }
            }
            crate::error::ParsingError::Failed { kind, message } => {
                crate::Error::Malformed { kind, message }
            }
        })
    }

    /// Parse Exif metadata from an in-memory byte payload built via
    /// the deprecated [`MediaSource::<()>::from_bytes`].
    ///
    /// **Deprecated since v3.3.0**: use [`Self::parse_exif`] with
    /// [`MediaSource::from_memory`] directly.
    #[deprecated(
        since = "3.3.0",
        note = "Use `parse_exif` directly — it now accepts memory-mode \
                sources built via `MediaSource::from_memory`."
    )]
    pub fn parse_exif_from_bytes(&mut self, ms: MediaSource<()>) -> crate::Result<ExifIter> {
        self.parse_exif(ms.into_empty())
    }

    /// **Deprecated since v3.3.0**: use [`Self::parse_track`] with
    /// [`MediaSource::from_memory`] directly.
    #[deprecated(
        since = "3.3.0",
        note = "Use `parse_track` with `MediaSource::from_memory`."
    )]
    pub fn parse_track_from_bytes(&mut self, ms: MediaSource<()>) -> crate::Result<TrackInfo> {
        self.parse_track(ms.into_empty())
    }

    /// Parse all metadata from an image source: EXIF (if any) and
    /// format-specific extras (PNG `tEXt` chunks, etc.).
    ///
    /// Returns `Err(Error::ExifNotFound)` if neither EXIF nor any
    /// format-specific metadata is found. Returns
    /// `Err(Error::TrackNotFound)`-style errors on track inputs (use
    /// `parse_track` instead).
    ///
    /// **Lazy form** — this method returns `ImageMetadata<ExifIter>`.
    /// Convert to the eager `ImageMetadata<Exif>` via `.into()` if
    /// desired.
    pub fn parse_image_metadata<R: Read>(
        &mut self,
        mut ms: MediaSource<R>,
    ) -> crate::Result<crate::image_metadata::ImageMetadata<crate::ExifIter>> {
        self.reset();
        let res: crate::Result<crate::image_metadata::ImageMetadata<crate::ExifIter>> = (|| {
            // Reject track inputs early (parse_track is the right API).
            let mime_img = match ms.mime {
                crate::file::MediaMime::Image(img) => img,
                crate::file::MediaMime::Track(_) => return Err(crate::Error::ExifNotFound),
            };

            // Memory-mode shortcut + buffer setup mirrors parse_exif.
            if let Some(memory) = ms.memory.take() {
                self.state.set_memory(memory);
            } else {
                self.acquire_buf();
                self.buf_mut().append(&mut ms.buf);
                // PNG-only EOF tolerance: a tEXt-only PNG can be smaller than
                // HEADER_PARSE_BUF_SIZE (e.g. 117-byte text-only.png) so the
                // mime-detection prefill consumes the whole reader and
                // fill_buf returns UnexpectedEof. The bytes we need are
                // already in the parse buffer — proceed. Other formats keep
                // the strict-EOF contract.
                let is_png = matches!(
                    mime_img,
                    crate::file::MediaMimeImage::Png | crate::file::MediaMimeImage::Webp
                );
                match self.fill_buf(&mut ms.reader, INIT_BUF_SIZE) {
                    Ok(_) => {}
                    Err(e)
                        if is_png
                            && !self.buffer().is_empty()
                            && e.kind() == io::ErrorKind::UnexpectedEof => {}
                    Err(e) => return Err(e.into()),
                }
            }

            if mime_img == crate::file::MediaMimeImage::Png {
                let (exif, text_chunks) =
                    crate::exif::parse_png_full(self, &mut ms.reader, ms.skip_by_seek)?;
                let format = if text_chunks.is_empty() {
                    None
                } else {
                    Some(crate::ImageFormatMetadata::Png(crate::PngTextChunks {
                        entries: text_chunks,
                    }))
                };
                if exif.is_none() && format.is_none() {
                    return Err(crate::Error::ExifNotFound);
                }
                Ok(crate::ImageMetadata { exif, format })
            } else {
                // Non-PNG: existing parse_exif_iter path; format always None.
                let iter =
                    crate::exif::parse_exif_iter(self, mime_img, &mut ms.reader, ms.skip_by_seek)?;
                Ok(crate::ImageMetadata {
                    exif: Some(iter),
                    format: None,
                })
            }
        })(
        );
        self.reset();
        res
    }

    fn reset(&mut self) {
        self.state.reset();
    }

    fn buf_mut(&mut self) -> &mut Vec<u8> {
        self.state.buf_mut()
    }

    fn acquire_buf(&mut self) {
        self.state.acquire_buf();
    }
}

#[cfg(feature = "tokio")]
mod tokio_impl {
    use super::*;
    use crate::error::ParsingErrorState;
    use crate::parser_async::{AsyncBufParser, AsyncMediaSource};
    use tokio::io::{AsyncRead, AsyncReadExt};

    impl AsyncBufParser for MediaParser {
        async fn fill_buf<R: AsyncRead + Unpin>(
            &mut self,
            reader: &mut R,
            size: usize,
        ) -> std::io::Result<usize> {
            if self.state.is_memory_mode() {
                // Memory mode owns every byte it will ever have. Surface
                // "walked off end of input" the same way the streaming path
                // surfaces a 0-byte read.
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            check_fill_size(self.state.buf().len(), size)?;
            // Same rationale as the sync version: do not pre-allocate `size` bytes.
            let n = reader
                .take(size as u64)
                .read_to_end(self.state.buf_mut())
                .await?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            Ok(n)
        }
    }

    impl MediaParser {
        /// Parse Exif metadata from an async image source. Returns
        /// `Error::ExifNotFound` if the source is a `Track`.
        ///
        /// As of v3.3, also accepts memory-mode sources built via
        /// [`AsyncMediaSource::from_memory`]; the memory branch shares
        /// caller-owned `Bytes` zero-copy through `state.set_memory`.
        pub async fn parse_exif_async<R: AsyncRead + Unpin + Send>(
            &mut self,
            mut ms: AsyncMediaSource<R>,
        ) -> crate::Result<ExifIter> {
            self.reset();
            let res: crate::Result<ExifIter> = async {
                if let Some(memory) = ms.memory.take() {
                    // Memory-mode: zero-copy share of caller-owned bytes.
                    self.state.set_memory(memory);
                    if !matches!(ms.mime, crate::file::MediaMime::Image(_)) {
                        return Err(crate::Error::ExifNotFound);
                    }
                    crate::exif::parse_exif_iter_async(
                        self,
                        ms.mime.unwrap_image(),
                        &mut ms.reader,
                        ms.skip_by_seek,
                    )
                    .await
                } else {
                    self.acquire_buf();
                    self.buf_mut().append(&mut ms.buf);
                    // PNG-only EOF tolerance mirrors the sync path: small
                    // tEXt-only PNGs (<HEADER_PARSE_BUF_SIZE) are fully
                    // consumed during mime detection, so fill_buf returns
                    // UnexpectedEof. The bytes are already in the parse
                    // buffer; proceed.
                    let is_png = matches!(
                        ms.mime,
                        crate::file::MediaMime::Image(
                            crate::file::MediaMimeImage::Png | crate::file::MediaMimeImage::Webp
                        )
                    );
                    match <Self as AsyncBufParser>::fill_buf(self, &mut ms.reader, INIT_BUF_SIZE)
                        .await
                    {
                        Ok(_) => {}
                        Err(e)
                            if is_png
                                && !self.buffer().is_empty()
                                && e.kind() == io::ErrorKind::UnexpectedEof => {}
                        Err(e) => return Err(e.into()),
                    }
                    if !matches!(ms.mime, crate::file::MediaMime::Image(_)) {
                        return Err(crate::Error::ExifNotFound);
                    }
                    crate::exif::parse_exif_iter_async(
                        self,
                        ms.mime.unwrap_image(),
                        &mut ms.reader,
                        ms.skip_by_seek,
                    )
                    .await
                }
            }
            .await;
            self.reset();
            res
        }

        pub async fn parse_image_metadata_async<R: AsyncRead + Unpin + Send>(
            &mut self,
            mut ms: AsyncMediaSource<R>,
        ) -> crate::Result<crate::image_metadata::ImageMetadata<crate::ExifIter>> {
            self.reset();
            let res: crate::Result<crate::image_metadata::ImageMetadata<crate::ExifIter>> = async {
                let mime_img = match ms.mime {
                    crate::file::MediaMime::Image(img) => img,
                    crate::file::MediaMime::Track(_) => return Err(crate::Error::ExifNotFound),
                };

                if let Some(memory) = ms.memory.take() {
                    self.state.set_memory(memory);
                } else {
                    self.acquire_buf();
                    self.buf_mut().append(&mut ms.buf);
                    // PNG-only EOF tolerance mirrors the sync path: small
                    // tEXt-only PNGs (<HEADER_PARSE_BUF_SIZE) are fully
                    // consumed during mime detection, so fill_buf returns
                    // UnexpectedEof; the bytes we need are already in the
                    // parse buffer.
                    let is_png = matches!(
                        mime_img,
                        crate::file::MediaMimeImage::Png | crate::file::MediaMimeImage::Webp
                    );
                    match <Self as AsyncBufParser>::fill_buf(self, &mut ms.reader, INIT_BUF_SIZE)
                        .await
                    {
                        Ok(_) => {}
                        Err(e)
                            if is_png
                                && !self.buffer().is_empty()
                                && e.kind() == io::ErrorKind::UnexpectedEof => {}
                        Err(e) => return Err(e.into()),
                    }
                }

                if mime_img == crate::file::MediaMimeImage::Png {
                    let (exif, text_chunks) =
                        crate::exif::parse_png_full_async(self, &mut ms.reader, ms.skip_by_seek)
                            .await?;
                    let format = if text_chunks.is_empty() {
                        None
                    } else {
                        Some(crate::ImageFormatMetadata::Png(crate::PngTextChunks {
                            entries: text_chunks,
                        }))
                    };
                    if exif.is_none() && format.is_none() {
                        return Err(crate::Error::ExifNotFound);
                    }
                    Ok(crate::image_metadata::ImageMetadata { exif, format })
                } else {
                    let iter = crate::exif::parse_exif_iter_async(
                        self,
                        mime_img,
                        &mut ms.reader,
                        ms.skip_by_seek,
                    )
                    .await?;
                    Ok(crate::image_metadata::ImageMetadata {
                        exif: Some(iter),
                        format: None,
                    })
                }
            }
            .await;
            self.reset();
            res
        }

        /// Parse track info from an async video/audio source. Returns
        /// `Error::TrackNotFound` if the source is an `Image`.
        ///
        /// As of v3.3, also accepts memory-mode sources built via
        /// [`AsyncMediaSource::from_memory`].
        pub async fn parse_track_async<R: AsyncRead + Unpin + Send>(
            &mut self,
            mut ms: AsyncMediaSource<R>,
        ) -> crate::Result<TrackInfo> {
            self.reset();
            let res: crate::Result<TrackInfo> = async {
                let mime_track = match ms.mime {
                    crate::file::MediaMime::Image(_) => return Err(crate::Error::TrackNotFound),
                    crate::file::MediaMime::Track(t) => t,
                };
                if let Some(memory) = ms.memory.take() {
                    self.state.set_memory(memory);
                } else {
                    self.acquire_buf();
                    self.buf_mut().append(&mut ms.buf);
                    <Self as AsyncBufParser>::fill_buf(self, &mut ms.reader, INIT_BUF_SIZE).await?;
                }
                let skip = ms.skip_by_seek;
                let out = <Self as AsyncBufParser>::load_and_parse(
                    self,
                    &mut ms.reader,
                    skip,
                    |data, _| {
                        crate::video::parse_track_info(data, mime_track)
                            .map_err(|e| ParsingErrorState::new(e, None))
                    },
                )
                .await?;
                Ok(out)
            }
            .await;
            self.reset();
            res
        }
    }
}
