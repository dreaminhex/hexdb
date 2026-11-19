# Image findings: triage

Each release scans its images on both architectures (`.github/workflows/release.yml`). The release stops on a critical finding with a fix available. Everything else is in the full report, kept as the `trivy-image` artifact of each release run, and triaged here.

Owner of every entry: the HexDB maintainers. Review by the date given, or at the next release whose report changes the picture, whichever is first. An entry whose packages get a fix is dropped: the release picks up fixed Debian packages itself (`apt-get upgrade` in the image).

Last triaged: the 2.5.9 report (Debian 13.7 base), 2026-11-19: 1 critical, 155 high, 239 medium, 211 low, 39 unknown. New since 2.5.8: libacl1 (high), and unknown-severity entries in glibc's iconv, GnuTLS, OpenSSL, OpenJPEG, expat and Python (rows below). Every finding is in a Debian package; none has a fixed version yet. None is in HexDB's own code or its Rust dependencies (those are checked by `cargo audit` in CI).

## Where a server-only deployment doesn't need the package

Since 2.5.9 there is a slim image (`hexdb:<version>-slim`): the server, the CLI and the admin UI on a minimal Debian, without Python, Node.js, Tesseract, ffmpeg or curl. Script functions are off in it, and OCR and the thumbnails only ffmpeg makes aren't available. It drops most of the findings below. Use it unless you need those features.

## Accepted risks (full image)

| Packages | Findings | Why accepted | Review by |
| --- | --- | --- | --- |
| tesseract-ocr, libtesseract5, libtiff6 | high: memory corruption from crafted images | Reached only through OCR of uploaded files, which runs in the process sandbox: no network, no writable folder, HexDB's data, configuration and keys out of reach, memory and CPU limits, one process, a deadline (see MANUAL, "OCR"). OCR is off unless Tesseract is found; the slim image doesn't have it. | 2027-01-31 |
| libxml2 (critical CVE-2026-6653, high) | crafted XML | Not used by HexDB. Pulled in by Tesseract's and Python's dependencies; reachable only from OCR (sandboxed, above) or administrators' own scripts. | 2027-01-31 |
| python3.13, libpython3.13, libexpat1 | high: parsers (HTML, XML, tarfile), TLS | Python runs only script functions and plugins, which administrators write and register; it doesn't parse anything for HexDB. | 2027-01-31 |
| curl, libcurl4t64 | high: SSH host verification, connection reuse | Used by the image's health check against 127.0.0.1 over HTTP, and available to administrators' scripts. | 2027-01-31 |
| krb5 libraries (libkrb5-3, libgssapi-krb5-2, libk5crypto3, libkrb5support0) | high: KDC and kadmind | The image runs no KDC or kadmind; the libraries are dependencies of curl and Python. | 2027-03-31 |
| util-linux and its libraries, login, mount, libsystemd0, libudev1, libacl1 | high: mount helpers, nsenter, systemd-homed, ACL symlink traversal | Base system tools. The container runs no mounts, no systemd and no logins; HexDB runs as an unprivileged user. | 2027-03-31 |
| libx11, libxrender1, ncurses, perl-base, sqlite | high: X servers, terminals, Archive::Tar, FTS5 | Libraries pulled in as dependencies; HexDB uses none of them and they process no input from it. | 2027-03-31 |
| libssl3t64, openssl, libgnutls30t64 | unknown: DANE and DTLS client crashes, GnuTLS advisories of 2026-11-18 | HexDB uses rustls for TLS, not OpenSSL or GnuTLS. The libraries are dependencies of curl and Python. | 2027-01-31 |
| libc6, libc-bin (iconv) | unknown: crafted input to a few legacy character-set conversions | HexDB doesn't call iconv; its text handling is Rust's UTF-8. | 2027-03-31 |
| libopenjp2-7 | unknown: heap overflow decoding JPEG 2000 | Pulled in by Tesseract's image library and reached only from OCR (sandboxed, above). HexDB's own image previews are decoded in Rust. | 2027-01-31 |
| Everything medium, low and unknown | | Same components as above; reviewed together with them. | 2027-03-31 |

## Process

1. After a release, download `trivy-image` from its run and compare it with the table above.
2. A new finding in a component not listed, or a listed one now reachable from requests, gets a row (or a fix) before the next release.
3. A critical finding with a fix available already stops the release; a high one with a fix should be picked up by rebuilding (a patch release) within 30 days.
