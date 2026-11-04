# hexdb (Python)

Client for HexDB. Python 3.9+, standard library only. See ../README.md and the HexDB manual.

## Connection URIs

`HexDB` takes a connection URI wherever it takes a URL:

```python
from hexdb import HexDB, parse_connection_uri

db = HexDB("hexdbs://analyst:s3cret%21@db.example.com")      # signs in with POST /auth/login
db = HexDB("hexdb://127.0.0.1:7700?apiKey=hxk_...&timeout=10")
```

- `hexdb://` is HTTP, port 7700 by default. `hexdbs://` is HTTPS, port 443 by default.
- A login in the URI signs in when `HexDB` is created. Percent-encode `@`, `:` and `/` in the login and password.
- The `apiKey`, `timeout` and `caFile` options set the API key, the request timeout in seconds and the CA file. The `api_key`, `timeout` and `ca_file` arguments win over them; an explicit `api_key` also skips the sign-in. `pageSize` is parsed but not used by this driver.
- A path is kept as the base of every request (`hexdbs://example.com/hexdb`).
- An invalid URI raises `ValueError` naming the problem.
- Plain `http://` and `https://` URLs work as before.

`parse_connection_uri(uri)` returns a `ConnectionUri` with `url`, `login`, `password`, `api_key`, `timeout_seconds`, `page_size` and `ca_file` (None when absent). See "Connection URIs" in the HexDB manual for the full format.
