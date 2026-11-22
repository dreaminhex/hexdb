# Third-party notices

HexDB is licensed under the Apache License 2.0 (see LICENSE). It includes the following third-party code, whose licenses require this notice in source and binary distributions.

## OpenH264

HexDB builds Cisco's OpenH264 decoder from source (through the `openh264` crate) to make thumbnails from H.264 video. It is included unless HexDB is built with `--no-default-features`.

https://github.com/cisco/openh264

```
Copyright (c) 2013, Cisco Systems
All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice, this
  list of conditions and the following disclaimer in the documentation and/or
  other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## nom-exif (modified)

HexDB builds a modified copy of nom-exif 3.8.0 (https://github.com/mindeng/nom-exif, MIT license, copyright (c) 2024 Min Deng), in `vendor/nom-exif`, with its date and time handling moved from chrono to jiff. The license text is in `vendor/nom-exif/LICENSE`; the changes are listed in `vendor/VENDORED.md`.

## In the Docker image

The image (`Dockerfile`) also carries programs HexDB runs, each as a separate process, never linked into HexDB:

- **FFmpeg** (https://ffmpeg.org), for thumbnails of HEIC and AVIF photos and of HEVC, VP9 and AV1 video. It is licensed under the GNU Lesser General Public License, version 2.1 or later; the license text is in the image at `/usr/share/doc/ffmpeg/COPYING.LGPLv2.1`. It is built by `docker/build-ffmpeg.sh` from Debian trixie's `ffmpeg` source package (`apt-get source ffmpeg`, also at https://sources.debian.org/src/ffmpeg/), without `--enable-gpl` or `--enable-nonfree` and with only the components that script lists; running the script builds it again. It uses dav1d (BSD 2-clause) for AV1, from Debian's `libdav1d7` package.
- **Node.js** (https://nodejs.org), for JavaScript and TypeScript functions and process plugins: the `node` program from the official `node:24` image, under the MIT license, with the licenses of the components it bundles in the same file, in the image at `/usr/share/doc/nodejs/LICENSE`. (Before 2.5.6 the image used Debian's `nodejs` package.)
- **Python** 3 (https://www.python.org), for Python functions and plugins, from Debian's `python3` package, under the Python Software Foundation License.
- **Tesseract OCR** (https://github.com/tesseract-ocr/tesseract), Apache License 2.0, with its English language data, from Debian's `tesseract-ocr` and `tesseract-ocr-eng` packages.

Debian packages keep their copyright and license files under `/usr/share/doc/<package>/` in the image.
