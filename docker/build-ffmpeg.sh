#!/usr/bin/env bash
# Builds the small ffmpeg HexDB's image carries, for thumbnails of HEIC and AVIF
# photos and of video HexDB doesn't decode itself (see hexdb_core/src/tools.rs).
#
#   docker run --rm -v "$PWD/out:/out" -v "$PWD/docker:/scripts:ro" debian:trixie bash /scripts/build-ffmpeg.sh /out
#
# Run on Debian trixie (the runtime image's release, so the libraries match),
# natively on the target architecture. It builds Debian's own ffmpeg source
# (fetched and verified by apt) with everything off but what HexDB uses: input
# from a pipe; the MP4/QuickTime/HEIF, Matroska/WebM, AVI, MPEG-TS/PS, FLV, Ogg
# and ASF containers and raw HEVC/H.264/AV1/IVF streams; HEVC, H.264, VP8, VP9,
# AV1 (dav1d), MPEG-1/2/4, WMV/VC-1 and Theora decoders; scaling, rotation and
# tiling filters; and a JPEG encoder for the thumbnail. Fewer decoders mean
# less of ffmpeg for a hostile file to reach. The result needs only libdav1d
# and zlib at run time; `ldd` is printed to show it.
set -euo pipefail
out="${1:?usage: build-ffmpeg.sh <output folder>}"
export DEBIAN_FRONTEND=noninteractive

sed -i 's/^Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/debian.sources
apt-get update -qq
apt-get install -y -qq --no-install-recommends build-essential nasm pkg-config ca-certificates dpkg-dev libdav1d-dev zlib1g-dev >/dev/null
work=$(mktemp -d)
cd "$work"
apt-get source -qq ffmpeg >/dev/null
cd ffmpeg-*/

list() { local IFS=,; echo "$*"; }
./configure \
  --prefix=/usr/local \
  --disable-everything --disable-autodetect --disable-doc --disable-debug --disable-network \
  --disable-ffplay --disable-ffprobe --disable-avdevice --disable-swresample \
  --enable-avfilter --enable-swscale --enable-zlib --enable-libdav1d \
  --enable-protocol=pipe \
  --enable-demuxer="$(list mov matroska avi mpegts mpegps flv ogg asf hevc h264 obu ivf m4v mpegvideo)" \
  --enable-decoder="$(list hevc h264 vp8 vp9 libdav1d mpeg4 mpeg1video mpeg2video msmpeg4v3 wmv1 wmv2 wmv3 vc1 theora mjpeg)" \
  --enable-parser="$(list hevc h264 vp8 vp9 av1 mpeg4video mpegvideo vc1)" \
  --enable-bsf="$(list hevc_mp4toannexb h264_mp4toannexb)" \
  --enable-encoder=mjpeg \
  --enable-muxer="$(list image2pipe mjpeg)" \
  --enable-filter="$(list buffer buffersink format null scale transpose hflip vflip rotate crop pad overlay xstack tile setsar)" \
  >/dev/null
make -j"$(nproc)" >/dev/null
strip ffmpeg
mkdir -p "$out"
cp ffmpeg "$out/ffmpeg"
# Built without --enable-gpl or --enable-nonfree: LGPL 2.1 or later, whose text goes with the binary.
cp COPYING.LGPLv2.1 "$out/COPYING.LGPLv2.1"
"$out/ffmpeg" -hide_banner -version | head -1
ldd "$out/ffmpeg"
ls -lh "$out/ffmpeg"
