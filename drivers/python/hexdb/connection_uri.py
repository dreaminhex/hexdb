"""Connection URIs: hexdb://[login[:password]@]host[:port][/path][?option=value&...].

`hexdb://` is HTTP (port 7700 by default), `hexdbs://` is HTTPS (port 443 by default).
See "Connection URIs" in the HexDB manual.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Dict, List, Optional, Tuple

__all__ = ["ConnectionUri", "parse_connection_uri", "is_connection_uri"]

_SCHEMES = {"hexdb": ("http", 7700), "hexdbs": ("https", 443)}
_HEX = "0123456789abcdefABCDEF"
_HOST_CHARS = set("abcdefghijklmnopqrstuvwxyz0123456789-._")
_IPV6_CHARS = set("0123456789abcdef:.")


@dataclass(frozen=True)
class ConnectionUri:
    """A parsed connection URI. Absent parts are None; `url` has no trailing slash."""

    url: str
    login: Optional[str] = None
    password: Optional[str] = None
    api_key: Optional[str] = None
    timeout_seconds: Optional[int] = None
    page_size: Optional[int] = None
    ca_file: Optional[str] = None


def is_connection_uri(text: str) -> bool:
    """True when `text` starts with the hexdb: or hexdbs: scheme (any case)."""
    scheme, sep, _ = text.partition(":")
    return bool(sep) and scheme.lower() in _SCHEMES


def _fail(problem: str) -> ValueError:
    return ValueError(f"Invalid HexDB connection URI: {problem}")


def _decode(text: str, what: str) -> str:
    """Percent-decode `text` strictly: every % starts two hex digits, and the bytes are UTF-8."""
    if "%" not in text:
        return text
    out = bytearray()
    i = 0
    while i < len(text):
        c = text[i]
        if c == "%":
            pair = text[i + 1 : i + 3]
            if len(pair) != 2 or pair[0] not in _HEX or pair[1] not in _HEX:
                raise _fail(f"{what} has a % that isn't followed by two hex digits")
            out.append(int(pair, 16))
            i += 3
        else:
            out.extend(c.encode("utf-8"))
            i += 1
    try:
        return out.decode("utf-8")
    except UnicodeDecodeError:
        raise _fail(f"{what} isn't valid UTF-8 once percent-decoded") from None


def _int_option(name: str, value: str, low: int, high: Optional[int]) -> int:
    if not value.isascii() or not value.isdigit():
        raise _fail(f"{name} isn't a positive integer: {value!r}")
    number = int(value)
    if number < low or (high is not None and number > high):
        if high is None:
            raise _fail(f"{name} isn't a positive integer: {value!r}")
        raise _fail(f"{name} is outside {low} to {high:,}: {value}")
    return number


def _host_port(hostport: str) -> Tuple[str, Optional[str]]:
    """Split host[:port], with IPv6 in brackets. Returns (host, port text or None)."""
    if "," in hostport:
        raise _fail("more than one host; give one host (a name that reaches several hexes)")
    if hostport.startswith("["):
        end = hostport.find("]")
        if end < 0:
            raise _fail("an IPv6 address has no closing ]")
        address = hostport[1:end].lower()
        if not address or not set(address) <= _IPV6_CHARS or ":" not in address:
            raise _fail(f"the IPv6 address [{address}] isn't valid")
        rest = hostport[end + 1 :]
        if rest and not rest.startswith(":"):
            raise _fail(f"unexpected {rest!r} after the IPv6 address")
        return f"[{address}]", (rest[1:] if rest else None)
    host, sep, port = hostport.partition(":")
    if sep and ":" in port:
        raise _fail("an IPv6 host must be in brackets, as in [::1]")
    host = host.lower()
    if not host:
        raise _fail("no host")
    if not set(host) <= _HOST_CHARS:
        raise _fail(f"the host {host!r} has characters a host name can't have")
    return host, (port if sep else None)


def parse_connection_uri(uri: str) -> ConnectionUri:
    """Parse a hexdb:// or hexdbs:// URI. Raises ValueError naming the problem when it isn't valid."""
    if not isinstance(uri, str):
        raise TypeError("a connection URI is a string")
    scheme, sep, rest = uri.partition(":")
    scheme = scheme.lower()
    if not sep or scheme not in _SCHEMES:
        raise _fail("not a hexdb:// or hexdbs:// URI")
    if not rest.startswith("//"):
        raise _fail(f"no // after {scheme}:")
    rest = rest[2:]
    if "#" in rest:
        raise _fail("a fragment (#...) isn't allowed")
    rest, _, query = rest.partition("?")
    slash = rest.find("/")
    authority, path = (rest, "") if slash < 0 else (rest[:slash], rest[slash:])

    login: Optional[str] = None
    password: Optional[str] = None
    at = authority.rfind("@")
    if at >= 0:
        userinfo, authority = authority[:at], authority[at + 1 :]
        raw_login, has_password, raw_password = userinfo.partition(":")
        login = _decode(raw_login, "the login")
        if not login:
            raise _fail("the login before @ is empty")
        if has_password:
            password = _decode(raw_password, "the password")

    if not authority:
        raise _fail("no host")
    host, port_text = _host_port(authority)
    http_scheme, default_port = _SCHEMES[scheme]
    port = default_port
    if port_text is not None:
        if not port_text:
            raise _fail("the port after : is empty")
        if not port_text.isascii() or not port_text.isdigit():
            raise _fail(f"the port isn't a number: {port_text!r}")
        port = int(port_text)
        if not 1 <= port <= 65535:
            raise _fail(f"the port is outside 1 to 65535: {port}")

    options: Dict[str, str] = {}
    pieces: List[str] = [p for p in query.split("&") if p] if query else []
    for piece in pieces:
        raw_name, _, raw_value = piece.partition("=")
        name = _decode(raw_name, "an option name").lower()
        options[name] = _decode(raw_value, f"the {name} option")

    api_key = options.get("apikey")
    if api_key is not None and not api_key:
        raise _fail("apiKey is empty")
    if api_key is not None and login is not None:
        raise _fail("a login and apiKey together; use one or the other")
    timeout = options.get("timeout")
    page_size = options.get("pagesize")
    ca_file = options.get("cafile")
    if ca_file is not None and not ca_file:
        raise _fail("caFile is empty")

    path = path.rstrip("/")
    port_part = "" if http_scheme == "https" and port == 443 else f":{port}"
    return ConnectionUri(
        url=f"{http_scheme}://{host}{port_part}{path}",
        login=login,
        password=password,
        api_key=api_key,
        timeout_seconds=None if timeout is None else _int_option("timeout", timeout, 1, None),
        page_size=None if page_size is None else _int_option("pageSize", page_size, 1, 10_000),
        ca_file=ca_file,
    )
