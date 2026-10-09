"""HexDB client for Python 3.9+. Standard library only.

    from hexdb import HexDB

    db = HexDB("http://127.0.0.1:7700", api_key=os.environ["HEXDB_API_KEY"])
    orders = db.tessellation("orders")
    doc = orders.insert({"customer": "ada", "total": 12})
    page = orders.query(filter={"total": {"$gt": 10}}, sort="-total")
"""

from __future__ import annotations

import json
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Callable, Dict, Iterator, List, Optional, Sequence, Union

__all__ = ["HexDB", "HexDBError", "Tessellation", "Stream"]
__version__ = "1.0.0"

Json = Any
Document = Dict[str, Any]


class HexDBError(Exception):
    """An error answer from HexDB: `status` is the HTTP status, `code` HexDB's error code."""

    def __init__(self, status: int, code: str, message: str, retry_after: Optional[float] = None):
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        self.retry_after = retry_after

    def __repr__(self) -> str:
        return f"HexDBError({self.status}, {self.code!r}, {self.message!r})"


def _quote(text: str) -> str:
    return urllib.parse.quote(text, safe="")


class HexDB:
    """A connection to a HexDB server (any hex; replicas forward writes)."""

    def __init__(
        self,
        url: str,
        api_key: Optional[str] = None,
        *,
        retries: int = 3,
        timeout: float = 30.0,
        ca_file: Optional[str] = None,
    ):
        self.base = url.rstrip("/")
        self.token = api_key
        self.retries = retries
        self.timeout = timeout
        self._context = ssl.create_default_context(cafile=ca_file) if ca_file else None

    # -- transport ---------------------------------------------------------

    def request(self, method: str, path: str, body: Json = None, headers: Optional[Dict[str, str]] = None, timeout: Optional[float] = None) -> Json:
        """Send a request to the REST API; returns the parsed JSON (None for 204)."""
        headers = dict(headers or {})
        data = None
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        headers["Accept"] = "application/json"
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        attempt = 0
        while True:
            req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
            try:
                with urllib.request.urlopen(req, timeout=timeout or self.timeout, context=self._context) as response:
                    if response.status == 204:
                        return None
                    text = response.read().decode("utf-8")
                    return json.loads(text) if text else None
            except urllib.error.HTTPError as e:
                text = e.read().decode("utf-8", "replace")
                e.close()
                retry_after = float(e.headers.get("Retry-After") or 0) or None
                retryable = e.code in (429, 503) and (method == "GET" or "Idempotency-Key" in headers)
                if retryable and attempt < self.retries:
                    time.sleep(retry_after or 2**attempt)
                    attempt += 1
                    continue
                try:
                    error = json.loads(text).get("error", {})
                except ValueError:
                    error = {}
                raise HexDBError(e.code, error.get("code", "error"), error.get("message", text or e.reason), retry_after) from None
            except urllib.error.URLError as e:
                raise HexDBError(0, "unreachable", f"Couldn't reach HexDB at {self.base}: {e.reason}") from None

    # -- session -----------------------------------------------------------

    def login(self, login: str, password: str, code: Optional[str] = None) -> Dict[str, Any]:
        """Sign in (with a one-time code if MFA is on); later requests use the session."""
        body: Dict[str, Any] = {"login": login, "password": password, "return_token": True}
        if code:
            body["code"] = code
        result = self.request("POST", "/auth/login", body)
        self.token = result["token"]
        return {"user": result["user"], "expires_at": result["expires_at"]}

    def logout(self) -> None:
        self.request("POST", "/auth/logout")
        self.token = None

    # -- server ------------------------------------------------------------

    def health(self) -> Dict[str, Any]:
        return self.request("GET", "/health")

    def status(self) -> Dict[str, Any]:
        return self.request("GET", "/status")

    def tessellation(self, name: str) -> "Tessellation":
        return Tessellation(self, name)

    def tessellations(self) -> List[Dict[str, Any]]:
        return self.request("GET", "/tessellations")["tessellations"]

    def transaction(self, operations: List[Dict[str, Any]], idempotency_key: Optional[str] = None) -> Dict[str, Any]:
        """Operations across tessellations, all or nothing."""
        return self.request("POST", "/transactions", {"operations": operations}, _idem(idempotency_key))

    def graphql(self, query: str, variables: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
        """Run a GraphQL query or mutation; raises HexDBError on GraphQL errors."""
        result = self.request("POST", "/graphql", {"query": query, "variables": variables or {}})
        if result.get("errors"):
            first = result["errors"][0]
            raise HexDBError(200, first.get("extensions", {}).get("code", "GRAPHQL"), first["message"])
        return result["data"]

    def changes(self, after: Optional[int] = None, tessellation: Optional[str] = None, stop: Optional[Callable[[], bool]] = None) -> Iterator[Dict[str, Any]]:
        """Committed changes after `after` (default: from now), as they happen (long polling)."""
        cursor = after
        while not (stop and stop()):
            params = {"wait": "30", "limit": "500"}
            if cursor is not None:
                params["after"] = str(cursor)
            if tessellation:
                params["tessellation"] = tessellation
            page = self.request("GET", "/changes?" + urllib.parse.urlencode(params), timeout=60)
            cursor = page["last_seq"]
            yield from page["changes"]

    def stream(self, name: str) -> "Stream":
        return Stream(self, name)

    def run_function(self, name: str, params: Optional[Dict[str, Any]] = None) -> Json:
        """Run a saved function; returns its result."""
        return self.request("POST", f"/functions/{_quote(name)}/run", {"params": params or {}})["result"]


def _idem(key: Optional[str]) -> Dict[str, str]:
    return {"Idempotency-Key": key} if key else {}


def _ttl(ttl_seconds: Optional[int]) -> str:
    return f"?ttl={int(ttl_seconds)}" if ttl_seconds else ""


class Tessellation:
    """A collection of documents."""

    def __init__(self, db: HexDB, name: str):
        self.db = db
        self.name = name
        self.path = "/" + _quote(name)

    def insert(self, doc: Document, *, ttl_seconds: Optional[int] = None, idempotency_key: Optional[str] = None) -> Document:
        return self.db.request("POST", self.path + _ttl(ttl_seconds), doc, _idem(idempotency_key))

    def insert_many(self, docs: Sequence[Document], *, ttl_seconds: Optional[int] = None, idempotency_key: Optional[str] = None) -> List[str]:
        """Insert up to 1000 documents atomically; returns their IDs in order."""
        return self.db.request("POST", f"{self.path}/_bulk{_ttl(ttl_seconds)}", list(docs), _idem(idempotency_key))["ids"]

    def get(self, id: str, fields: Optional[Sequence[str]] = None) -> Optional[Document]:
        query = "?fields=" + _quote(",".join(fields)) if fields else ""
        try:
            return self.db.request("GET", f"{self.path}/{_quote(id)}{query}")
        except HexDBError as e:
            if e.status == 404:
                return None
            raise

    def replace(self, id: str, doc: Document, *, ttl_seconds: Optional[int] = None, idempotency_key: Optional[str] = None) -> Document:
        return self.db.request("PUT", f"{self.path}/{_quote(id)}{_ttl(ttl_seconds)}", doc, _idem(idempotency_key))

    def patch(self, id: str, changes: Document, *, ttl_seconds: Optional[int] = None, idempotency_key: Optional[str] = None) -> Document:
        """Merge fields into a document (None removes a field)."""
        return self.db.request("PATCH", f"{self.path}/{_quote(id)}{_ttl(ttl_seconds)}", changes, _idem(idempotency_key))

    def delete(self, id: str, *, idempotency_key: Optional[str] = None) -> bool:
        try:
            self.db.request("DELETE", f"{self.path}/{_quote(id)}", headers=_idem(idempotency_key))
            return True
        except HexDBError as e:
            if e.status == 404:
                return False
            raise

    def query(
        self,
        filter: Optional[Dict[str, Any]] = None,
        sort: Union[str, List[Dict[str, Any]], None] = None,
        limit: int = 100,
        offset: int = 0,
        after: Optional[str] = None,
        fields: Optional[Sequence[str]] = None,
    ) -> Dict[str, Any]:
        """One page: {"documents", "total", "next", "plan"}."""
        body: Dict[str, Any] = {"limit": limit, "offset": offset}
        for key, value in (("filter", filter), ("sort", sort), ("after", after), ("fields", list(fields) if fields else None)):
            if value is not None:
                body[key] = value
        return self.db.request("POST", f"{self.path}/_query", body)

    def iterate(self, filter: Optional[Dict[str, Any]] = None, page_size: int = 500, fields: Optional[Sequence[str]] = None) -> Iterator[Document]:
        """Every matching document, page by page, in ID order."""
        after = None
        while True:
            page = self.query(filter=filter, limit=page_size, after=after, fields=fields)
            yield from page["documents"]
            if not page.get("next"):
                return
            after = page["next"]

    def count(self, filter: Optional[Dict[str, Any]] = None) -> int:
        query = "?filter=" + _quote(json.dumps(filter)) if filter else ""
        return self.db.request("GET", f"{self.path}/count{query}")["count"]

    def aggregate(self, group_by: Optional[List[str]] = None, aggregates: Optional[Dict[str, Any]] = None, **options: Any) -> Dict[str, Any]:
        """Group and summarize: aggregate(["status"], {"total": {"$sum": "amount"}}, filter=...)."""
        body = dict(options)
        if group_by is not None:
            body["group_by"] = group_by
        if aggregates is not None:
            body["aggregates"] = aggregates
        return self.db.request("POST", f"{self.path}/_aggregate", body)

    def upsert(self, key: Union[str, Sequence[str]], docs: Sequence[Document], *, idempotency_key: Optional[str] = None) -> Dict[str, Any]:
        """Insert or replace documents matched by key fields."""
        return self.db.request("POST", f"{self.path}/_upsert", {"key": key if isinstance(key, str) else list(key), "documents": list(docs)}, _idem(idempotency_key))

    def update_where(self, filter: Dict[str, Any], update: Dict[str, Any]) -> Dict[str, int]:
        return self.db.request("POST", f"{self.path}/_update", {"filter": filter, "update": update})


class Stream:
    """A publish/subscribe stream."""

    def __init__(self, db: HexDB, name: str):
        self.db = db
        self.name = name
        self.path = "/streams/" + _quote(name)

    def publish(self, *messages: Dict[str, Any]) -> List[str]:
        """Publish messages ({"payload", "key"?, "headers"?}); returns their offsets."""
        return self.db.request("POST", f"{self.path}/messages", list(messages))["offsets"]

    def read(self, after: Optional[str] = None, group: Optional[str] = None, limit: int = 100, wait: int = 0) -> Dict[str, Any]:
        params = {"limit": str(limit)}
        if after:
            params["after"] = after
        if group:
            params["group"] = group
        if wait:
            params["wait"] = str(wait)
        return self.db.request("GET", f"{self.path}/messages?" + urllib.parse.urlencode(params), timeout=wait + 30)

    def commit(self, group: str, offset: str) -> None:
        self.db.request("POST", f"{self.path}/groups/{_quote(group)}/commit", {"offset": offset})

    def consume(self, group: str, handle: Callable[[Dict[str, Any]], None], stop: Optional[Callable[[], bool]] = None, batch: int = 100) -> None:
        """Call `handle` for each message in order as a consumer group, committing after each batch."""
        while not (stop and stop()):
            page = self.read(group=group, limit=batch, wait=30)
            handled = None
            for message in page["messages"]:
                handle(message)
                handled = message["offset"]
                if stop and stop():
                    break
            if handled:
                self.commit(group, handled)
