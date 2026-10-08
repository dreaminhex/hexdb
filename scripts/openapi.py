"""Generates hexdb_api/openapi.json, the OpenAPI 3.1 description of the REST
API (served at GET /openapi.json). Edit the tables below when routes change,
then run:  python scripts/openapi.py
"""

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

OBJ = {"type": "object", "additionalProperties": True}
ERROR = {"$ref": "#/components/schemas/Error"}


def body(schema=None, description=None, required=True):
    return {"required": required, "description": description or "", "content": {"application/json": {"schema": schema or OBJ}}}


def ok(description="OK", schema=None, status="200"):
    return {status: {"description": description, "content": {"application/json": {"schema": schema or OBJ}}}}


def no_content(description="Done"):
    return {"204": {"description": description}}


def param(name, where="path", description="", schema=None, required=None):
    return {"name": name, "in": where, "required": where == "path" if required is None else required, "description": description, "schema": schema or {"type": "string"}}


TESS = param("tessellation", description="Tessellation name")
NAME = param("name", description="Name")
ID = param("id", description="Document ID (ULID)")
IDEM = param("Idempotency-Key", "header", "Makes the write safe to retry: the same key returns the first result.")
TTL = param("ttl", "query", "Expire the document after this many seconds.", {"type": "integer"})

# (method, path, tag, summary, extra)
ROUTES = [
    # Authentication
    ("post", "/auth/login", "Authentication", "Sign in", {"security": [], "requestBody": body({"type": "object", "required": ["login", "password"], "properties": {"login": {"type": "string"}, "password": {"type": "string"}, "code": {"type": "string", "description": "One-time code with MFA"}, "return_token": {"type": "boolean"}}}), "responses": ok("Signed in; sets the session cookie")}),
    ("post", "/auth/logout", "Authentication", "Sign out", {"responses": no_content()}),
    ("get", "/auth/me", "Authentication", "The signed-in user, roles and permissions", {"responses": ok()}),
    ("post", "/auth/password", "Authentication", "Change your password", {"requestBody": body({"type": "object", "properties": {"current_password": {"type": "string"}, "new_password": {"type": "string"}}}), "responses": no_content()}),
    ("get", "/auth/mfa", "Authentication", "Multi-factor authentication status", {"responses": ok()}),
    ("post", "/auth/mfa/setup", "Authentication", "Start enrolling an authenticator app", {"requestBody": body({"type": "object", "properties": {"password": {"type": "string"}}}), "responses": ok("The secret and an otpauth:// URI")}),
    ("post", "/auth/mfa/enable", "Authentication", "Confirm enrolment with a code; returns backup codes", {"requestBody": body({"type": "object", "properties": {"code": {"type": "string"}}}), "responses": ok()}),
    ("post", "/auth/mfa/disable", "Authentication", "Turn MFA off", {"requestBody": body({"type": "object", "properties": {"password": {"type": "string"}, "code": {"type": "string"}}}), "responses": no_content()}),
    ("post", "/auth/mfa/backup-codes", "Authentication", "Replace the backup codes", {"requestBody": body(), "responses": ok()}),
    ("get", "/auth/keys", "Authentication", "Your API keys (admins: ?all=true)", {"parameters": [param("all", "query", "", {"type": "boolean"})], "responses": ok()}),
    ("post", "/auth/keys", "Authentication", "Create an API key (shown once)", {"requestBody": body({"type": "object", "properties": {"name": {"type": "string"}, "expires_in_days": {"type": "integer"}}}), "responses": ok("Created", status="201")}),
    ("delete", "/auth/keys/{id}", "Authentication", "Revoke an API key", {"parameters": [param("id")], "responses": no_content()}),
    # Server
    ("get", "/health", "Server", "Liveness (identity and version for signed-in users)", {"security": [], "responses": ok()}),
    ("get", "/status", "Server", "Status and metrics (status permission)", {"responses": ok()}),
    ("get", "/status/history", "Server", "Metrics samples over time", {"parameters": [param("minutes", "query", "", {"type": "integer"})], "responses": ok()}),
    ("get", "/logs", "Server", "Recent server log records (logs permission)", {"parameters": [param(n, "query") for n in ("level", "after", "before", "q", "target", "limit")], "responses": ok()}),
    ("get", "/audit", "Server", "The audit trail, newest first (audit permission)", {"parameters": [param(n, "query") for n in ("actor", "action", "target", "outcome", "since", "until", "limit", "offset")], "responses": ok()}),
    ("get", "/settings", "Server", "Runtime settings and the effective configuration (admins)", {"responses": ok()}),
    ("put", "/settings", "Server", "Change settings (admins)", {"requestBody": body(description="{\"limits.max_document_kb\": 2048, ...}; null removes an override"), "responses": ok()}),
    ("post", "/join", "Server", "What another hex needs to join this lattice (admins, with password)", {"requestBody": body({"type": "object", "properties": {"password": {"type": "string"}}}), "responses": ok()}),
    ("get", "/plugins", "Server", "Plugins and their delivery state (plugins permission)", {"responses": ok()}),
    ("post", "/flush", "Server", "Flush memory to SSTables (maintenance permission)", {"responses": ok()}),
    ("post", "/compact", "Server", "Compact SSTables (maintenance permission)", {"responses": ok()}),
    ("post", "/shutdown", "Server", "Shut down gracefully (shutdown token or admin)", {"responses": {"202": {"description": "Shutting down"}}}),
    ("get", "/openapi.json", "Server", "This description", {"security": [], "responses": ok()}),
    # Tessellations
    ("get", "/tessellations", "Tessellations", "Tessellations you can read", {"responses": ok()}),
    ("post", "/tessellations", "Tessellations", "Create a tessellation", {"requestBody": body({"type": "object", "properties": {"name": {"type": "string"}}}), "responses": ok("Created", status="201")}),
    ("get", "/tessellations/{name}", "Tessellations", "A tessellation with its document count", {"parameters": [NAME], "responses": ok()}),
    ("delete", "/tessellations/{name}", "Tessellations", "Delete a tessellation and its documents (manage)", {"parameters": [NAME], "responses": no_content()}),
    ("get", "/tessellations/{name}/indexes", "Indexes", "Indexes with statistics", {"parameters": [NAME], "responses": ok()}),
    ("post", "/tessellations/{name}/indexes", "Indexes", "Create an index (manage)", {"parameters": [NAME], "requestBody": body({"type": "object", "required": ["fields"], "properties": {"fields": {"type": "array", "items": {"type": "string"}}, "name": {"type": "string"}, "kind": {"enum": ["field", "text"]}, "unique": {"type": "boolean"}, "analyzer": {"type": "string"}}}), "responses": ok("Created", status="201")}),
    ("delete", "/tessellations/{name}/indexes/{index}", "Indexes", "Drop an index (manage)", {"parameters": [NAME, param("index")], "responses": no_content()}),
    ("get", "/tessellations/{name}/advice", "Indexes", "Index suggestions from observed queries (?ai=true asks Claude)", {"parameters": [NAME, param("ai", "query", "", {"type": "boolean"})], "responses": ok()}),
    ("get", "/analyzers", "Indexes", "Text analyzers", {"responses": ok()}),
    ("post", "/analyzers/_analyze", "Indexes", "What an analyzer makes of some text", {"requestBody": body({"type": "object", "properties": {"analyzer": {"type": "string"}, "text": {"type": "string"}}}), "responses": ok()}),
    ("get", "/tessellations/{name}/schemas", "Schemas", "Schema versions and migration progress", {"parameters": [NAME], "responses": ok()}),
    ("post", "/tessellations/{name}/schemas", "Schemas", "Register a schema version (manage)", {"parameters": [NAME], "requestBody": body(description="{\"fields\": {...}, \"additional_fields\": true, \"migration\": [...]}"), "responses": ok("Created", status="201")}),
    ("delete", "/tessellations/{name}/schemas", "Schemas", "Remove every schema version (manage)", {"parameters": [NAME], "responses": no_content()}),
    ("post", "/tessellations/{name}/schemas/check", "Schemas", "Check a schema without registering it", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    # Documents
    ("get", "/{tessellation}", "Documents", "List documents (?filter=&sort=&limit=&offset=&after=&fields=)", {"parameters": [TESS] + [param(n, "query") for n in ("filter", "sort", "limit", "offset", "after", "fields")], "responses": ok()}),
    ("post", "/{tessellation}", "Documents", "Insert a document", {"parameters": [TESS, TTL, IDEM], "requestBody": body(), "responses": ok("Created", status="201")}),
    ("get", "/{tessellation}/count", "Documents", "Count documents (?filter=)", {"parameters": [TESS, param("filter", "query")], "responses": ok()}),
    ("post", "/{tessellation}/_query", "Documents", "Query: filter, sort, paging, projection", {"parameters": [TESS], "requestBody": body({"type": "object", "properties": {"filter": OBJ, "sort": {}, "limit": {"type": "integer"}, "offset": {"type": "integer"}, "after": {"type": "string"}, "fields": {}}}), "responses": ok()}),
    ("post", "/{tessellation}/_aggregate", "Documents", "Group and aggregate", {"parameters": [TESS], "requestBody": body(description="{\"filter\", \"group_by\", \"aggregates\": {\"total\": {\"$sum\": \"price\"}}, \"sort\", \"limit\"}"), "responses": ok()}),
    ("post", "/{tessellation}/_bulk", "Documents", "Insert up to 1000 documents atomically", {"parameters": [TESS, TTL, IDEM], "requestBody": body({"type": "array", "items": OBJ}), "responses": ok("Created", status="201")}),
    ("put", "/{tessellation}/_bulk", "Documents", "Replace several documents atomically", {"parameters": [TESS, IDEM], "requestBody": body({"type": "array", "items": OBJ}), "responses": ok()}),
    ("patch", "/{tessellation}/_bulk", "Documents", "Patch several documents atomically", {"parameters": [TESS, IDEM], "requestBody": body({"type": "array", "items": OBJ}), "responses": ok()}),
    ("post", "/{tessellation}/_upsert", "Documents", "Insert or replace documents matched by key fields", {"parameters": [TESS, IDEM], "requestBody": body({"type": "object", "properties": {"key": {}, "documents": {"type": "array", "items": OBJ}}}), "responses": ok()}),
    ("post", "/{tessellation}/_update", "Documents", "Patch every document matching a filter", {"parameters": [TESS, IDEM], "requestBody": body({"type": "object", "properties": {"filter": OBJ, "update": OBJ}}), "responses": ok()}),
    ("get", "/{tessellation}/{id}", "Documents", "A document (ETag: its version; ?fields=)", {"parameters": [TESS, ID, param("fields", "query")], "responses": ok()}),
    ("put", "/{tessellation}/{id}", "Documents", "Replace a document", {"parameters": [TESS, ID, TTL, IDEM], "requestBody": body(), "responses": ok()}),
    ("patch", "/{tessellation}/{id}", "Documents", "Merge fields into a document (null removes)", {"parameters": [TESS, ID, TTL, IDEM], "requestBody": body(), "responses": ok()}),
    ("delete", "/{tessellation}/{id}", "Documents", "Delete a document", {"parameters": [TESS, ID, IDEM], "responses": no_content()}),
    ("post", "/transactions", "Documents", "Operations across tessellations, all or nothing", {"parameters": [IDEM], "requestBody": body(description="{\"operations\": [{\"op\": \"insert|replace|patch|delete|get|check\", \"tessellation\", \"id\", \"data\", \"if_version\"}]}"), "responses": ok()}),
    ("post", "/graphql", "GraphQL", "Run a GraphQL query or mutation", {"requestBody": body({"type": "object", "properties": {"query": {"type": "string"}, "variables": OBJ, "operationName": {"type": "string"}}}), "responses": ok()}),
    # Changes
    ("get", "/changes", "Changes", "Committed changes after a sequence number (long poll with wait)", {"parameters": [param(n, "query") for n in ("after", "tessellation", "limit", "wait")], "responses": ok()}),
    ("get", "/changes/stream", "Changes", "Committed changes as Server-Sent Events", {"parameters": [param(n, "query") for n in ("after", "tessellation")], "responses": {"200": {"description": "text/event-stream"}}}),
    # Streams
    ("get", "/streams", "Streams", "Streams you can read", {"responses": ok()}),
    ("post", "/streams", "Streams", "Create a stream", {"requestBody": body(description="{\"name\", \"description\", \"retention_hours\", \"sources\": [...], \"destinations\": [...]}"), "responses": ok("Created", status="201")}),
    ("get", "/streams/{name}", "Streams", "Configuration, delivery status and consumer groups", {"parameters": [NAME], "responses": ok()}),
    ("put", "/streams/{name}", "Streams", "Replace the configuration", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("delete", "/streams/{name}", "Streams", "Delete a stream and its messages", {"parameters": [NAME], "responses": no_content()}),
    ("post", "/streams/{name}/messages", "Streams", "Publish one message or an array", {"parameters": [NAME], "requestBody": body(description="{\"payload\", \"key\", \"headers\"} or an array"), "responses": ok("Published", status="201")}),
    ("get", "/streams/{name}/messages", "Streams", "Read in order (long poll with wait)", {"parameters": [NAME] + [param(n, "query") for n in ("after", "group", "limit", "wait")], "responses": ok()}),
    ("post", "/streams/{name}/groups/{group}/commit", "Streams", "Commit a consumer group's offset", {"parameters": [NAME, param("group")], "requestBody": body({"type": "object", "properties": {"offset": {"type": "string"}}}), "responses": no_content()}),
    ("get", "/streams/{name}/subscribe", "Streams", "Messages as Server-Sent Events", {"parameters": [NAME] + [param(n, "query") for n in ("after", "group")], "responses": {"200": {"description": "text/event-stream"}}}),
    # Functions
    ("get", "/functions", "Functions", "Saved functions", {"responses": ok()}),
    ("post", "/functions", "Functions", "Create a function (admins)", {"requestBody": body(description="{\"name\", \"kind\": \"query|aggregate|transaction|script\", \"params\", \"tessellation\", \"body\", \"runtime\", \"code\"}"), "responses": ok("Created", status="201")}),
    ("get", "/functions/{name}", "Functions", "A function", {"parameters": [NAME], "responses": ok()}),
    ("put", "/functions/{name}", "Functions", "Replace a function (admins)", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("delete", "/functions/{name}", "Functions", "Delete a function (admins)", {"parameters": [NAME], "responses": no_content()}),
    ("post", "/functions/{name}/run", "Functions", "Run a function with your permissions", {"parameters": [NAME], "requestBody": body({"type": "object", "properties": {"params": OBJ}}, required=False), "responses": ok()}),
    ("get", "/schedules", "Functions", "Schedules (admins)", {"responses": ok()}),
    ("post", "/schedules", "Functions", "Create a schedule (admins)", {"requestBody": body(description="{\"name\", \"function\", \"params\", \"every_seconds\" | \"cron\", \"enabled\"}"), "responses": ok("Created", status="201")}),
    ("get", "/schedules/{name}", "Functions", "A schedule", {"parameters": [NAME], "responses": ok()}),
    ("put", "/schedules/{name}", "Functions", "Replace a schedule", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("delete", "/schedules/{name}", "Functions", "Delete a schedule", {"parameters": [NAME], "responses": no_content()}),
    ("post", "/schedules/{name}/run", "Functions", "Run a schedule now", {"parameters": [NAME], "responses": ok()}),
    # Users and roles
    ("get", "/users", "Users and roles", "Users (admins)", {"responses": ok()}),
    ("post", "/users", "Users and roles", "Create a user (admins)", {"parameters": [IDEM], "requestBody": body({"type": "object", "properties": {"login": {"type": "string"}, "password": {"type": "string"}, "email_address": {"type": "string"}, "roles": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}, "tessellations": {"type": "array", "items": {"type": "string"}}}}}}}), "responses": ok("Created", status="201")}),
    ("get", "/users/{user}", "Users and roles", "A user by ID or login", {"parameters": [param("user")], "responses": ok()}),
    ("put", "/users/{user}", "Users and roles", "Replace a user's editable fields", {"parameters": [param("user"), IDEM], "requestBody": body(), "responses": ok()}),
    ("patch", "/users/{user}", "Users and roles", "Change some of a user's fields", {"parameters": [param("user"), IDEM], "requestBody": body(), "responses": ok()}),
    ("delete", "/users/{user}", "Users and roles", "Delete a user", {"parameters": [param("user"), IDEM], "responses": no_content()}),
    ("get", "/roles", "Users and roles", "Roles and the permissions they can hold", {"responses": ok()}),
    ("post", "/roles", "Users and roles", "Create a custom role", {"requestBody": body({"type": "object", "properties": {"name": {"type": "string"}, "description": {"type": "string"}, "permissions": {"type": "array", "items": {"type": "string"}}}}), "responses": ok("Created", status="201")}),
    ("get", "/roles/{name}", "Users and roles", "A role", {"parameters": [NAME], "responses": ok()}),
    ("put", "/roles/{name}", "Users and roles", "Change a custom role", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("patch", "/roles/{name}", "Users and roles", "Change a custom role", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("delete", "/roles/{name}", "Users and roles", "Delete an unused custom role", {"parameters": [NAME], "responses": no_content()}),
]

TAGS = ["Authentication", "Documents", "Tessellations", "Indexes", "Schemas", "Changes", "Streams", "Functions", "GraphQL", "Users and roles", "Server"]


def build():
    paths = {}
    for method, path, tag, summary, extra in ROUTES:
        op = {"tags": [tag], "summary": summary, "operationId": f"{method}_{path.strip('/').replace('/', '_').replace('{', '').replace('}', '') or 'root'}"}
        op.update({k: v for k, v in extra.items() if k != "responses"})
        responses = dict(extra.get("responses", ok()))
        responses.setdefault("default", {"description": "An error: {\"error\": {\"code\", \"message\"}}", "content": {"application/json": {"schema": ERROR}}})
        op["responses"] = responses
        paths.setdefault(path, {})[method] = op
    return {
        "openapi": "3.1.0",
        "info": {
            "title": "HexDB REST API",
            "version": "0.1.0",
            "description": "HexDB's REST API. Authenticate with `Authorization: Bearer <API key or session token>` or the `hexdb_session` cookie (admin UI). Errors are `{\"error\": {\"code\", \"message\"}}`. See MANUAL.md for filters, aggregations, permissions and everything else.",
        },
        "servers": [{"url": "http://127.0.0.1:7700"}],
        "tags": [{"name": t} for t in TAGS],
        "security": [{"bearer": []}, {"session": []}],
        "components": {
            "securitySchemes": {
                "bearer": {"type": "http", "scheme": "bearer", "description": "An API key (hxk_...) or a session token (hxs....)."},
                "session": {"type": "apiKey", "in": "cookie", "name": "hexdb_session"},
            },
            "schemas": {
                "Error": {"type": "object", "properties": {"error": {"type": "object", "properties": {"code": {"type": "string"}, "message": {"type": "string"}}}}},
            },
        },
        "paths": paths,
    }


if __name__ == "__main__":
    out = ROOT / "hexdb_api" / "openapi.json"
    out.write_text(json.dumps(build(), indent=2) + "\n", encoding="utf-8")
    print(f"wrote {out} ({len(ROUTES)} operations)")
