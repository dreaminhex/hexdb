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
BINARY = {"type": "string", "format": "binary"}
FILE_PATH = {"name": "path", "in": "path", "required": True, "description": "folder/name, e.g. photos/2024/IMG_1.jpg", "schema": {"type": "string"}}
FILE_CONTENT = {"200": {"description": "The content", "content": {"application/octet-stream": {"schema": BINARY}}}, "206": {"description": "A byte range"}, "304": {"description": "Not modified"}, "416": {"description": "Range not satisfiable"}}

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
    ("get", "/config", "Configuration", "The lattice configuration: newest version, schema, and each hex's applied version (admins; secrets never returned)", {"responses": ok()}),
    ("put", "/config", "Configuration", "Change lattice settings; a new version (admins)", {"requestBody": body({"type": "object", "properties": {"changes": {"type": "object", "description": "setting key -> value; null clears"}, "comment": {"type": "string"}, "base_version": {"type": "integer"}}, "required": ["changes"]}), "responses": ok()}),
    ("get", "/config/history", "Configuration", "Configuration versions, newest first, with what each changed (admins)", {"parameters": [param(n, "query") for n in ("limit", "offset")], "responses": ok()}),
    ("get", "/config/versions/{version}", "Configuration", "One configuration version, secrets as fingerprints (admins)", {"parameters": [param("version", schema={"type": "integer"})], "responses": ok()}),
    ("post", "/config/rollback", "Configuration", "Save an earlier version's settings as a new version (admins)", {"requestBody": body({"type": "object", "properties": {"version": {"type": "integer"}, "comment": {"type": "string"}}, "required": ["version"]}), "responses": ok()}),
    ("get", "/ai", "Configuration", "The AI provider in use and this month's token usage and budget (admins)", {"responses": ok()}),
    ("post", "/ai/test", "Configuration", "A tiny call to the AI provider: ok, model, tokens, or the provider's error (admins)", {"responses": ok()}),
    ("post", "/ai/complete", "Configuration", "Run a prompt through the lattice's AI provider (ai permission); 429 over budget, 502 provider error", {"requestBody": body({"type": "object", "properties": {"prompt": {"type": "string"}, "system": {"type": "string"}, "max_tokens": {"type": "integer"}}, "required": ["prompt"]}), "responses": ok()}),
    ("get", "/ai/chat", "Configuration", "Whether the AI chat is on for the caller, and today's remaining allowance", {"responses": ok()}),
    ("post", "/ai/chat", "Configuration", "Ask the AI about the data you can read (chat permission); 429 over a chat limit, 502 provider error", {"requestBody": body({"type": "object", "properties": {"question": {"type": "string"}, "history": {"type": "array", "items": {"type": "object", "properties": {"role": {"type": "string", "enum": ["user", "assistant"]}, "content": {"type": "string"}}}}}, "required": ["question"]}), "responses": ok()}),
    ("get", "/auth/quota", "Authentication", "Your upload quota and what you use of it", {"responses": ok()}),
    ("get", "/queues", "Streams", "Queues you can read", {"responses": ok()}),
    ("post", "/queues", "Streams", "Create a queue (manage on queue:<name>)", {"requestBody": body({"type": "object", "properties": {"name": {"type": "string"}, "description": {"type": "string"}, "visibility_seconds": {"type": "integer"}, "max_receives": {"type": "integer"}, "dead_letter": {"type": "string"}, "retention_hours": {"type": "integer"}}, "required": ["name"]}), "responses": ok()}),
    ("get", "/queues/{name}", "Streams", "A queue's configuration and counts", {"parameters": [param("name")], "responses": ok()}),
    ("put", "/queues/{name}", "Streams", "Change a queue's configuration", {"parameters": [param("name")], "requestBody": body({"type": "object"}), "responses": ok()}),
    ("delete", "/queues/{name}", "Streams", "Delete a queue and its messages", {"parameters": [param("name")], "responses": ok()}),
    ("post", "/queues/{name}/messages", "Streams", "Send messages (write)", {"parameters": [param("name")], "requestBody": body({"type": "object", "properties": {"messages": {"type": "array", "items": {"type": "object", "properties": {"body": {}, "delay_seconds": {"type": "integer"}, "dedupe_key": {"type": "string"}}}}}}), "responses": ok()}),
    ("post", "/queues/{name}/receive", "Streams", "Lease ready messages (read); long-polls with wait_seconds", {"parameters": [param("name")], "requestBody": body({"type": "object", "properties": {"max": {"type": "integer"}, "visibility_seconds": {"type": "integer"}, "wait_seconds": {"type": "integer"}}}, required=False), "responses": ok()}),
    ("post", "/queues/{name}/ack", "Streams", "Acknowledge (delete) leased messages", {"parameters": [param("name")], "requestBody": body({"type": "object", "properties": {"receipts": {"type": "array", "items": {"type": "string"}}}}), "responses": ok()}),
    ("post", "/queues/{name}/nack", "Streams", "Give leased messages back", {"parameters": [param("name")], "requestBody": body({"type": "object"}), "responses": ok()}),
    ("post", "/queues/{name}/extend", "Streams", "Keep leased messages longer", {"parameters": [param("name")], "requestBody": body({"type": "object"}), "responses": ok()}),
    ("get", "/shares", "Files", "Your sharing links (?all=true: everyone's, administrators)", {"parameters": [param("all", "query", schema={"type": "boolean"})], "responses": ok()}),
    ("post", "/shares", "Files", "Make a sharing link to a file or folder (share permission)", {"requestBody": body({"type": "object", "properties": {"tessellation": {"type": "string"}, "file_id": {"type": "string"}, "folder": {"type": "string"}, "expires_in_days": {"type": "number"}, "password": {"type": "string"}, "max_downloads": {"type": "integer"}, "note": {"type": "string"}}, "required": ["tessellation"]}), "responses": ok()}),
    ("delete", "/shares/{code}", "Files", "Delete a sharing link (its maker or an administrator)", {"parameters": [param("code")], "responses": ok()}),
    ("get", "/_s/{code}", "Files", "A sharing link: what it is (no account needed; browsers are redirected to its page)", {"security": [], "parameters": [param("code")], "responses": ok()}),
    ("post", "/_s/{code}/unlock", "Files", "Unlock a password-protected link: a token for an hour", {"security": [], "parameters": [param("code")], "requestBody": body({"type": "object", "properties": {"password": {"type": "string"}}, "required": ["password"]}), "responses": ok()}),
    ("get", "/_s/{code}/content", "Files", "A shared file's content (?file=<id> in a folder link, ?t=<token>, ?download=true)", {"security": [], "parameters": [param("code"), param("file", "query"), param("t", "query"), param("download", "query", schema={"type": "boolean"})], "responses": ok()}),
    ("get", "/_s/{code}/thumbnail", "Files", "A shared file's thumbnail", {"security": [], "parameters": [param("code"), param("file", "query"), param("t", "query")], "responses": ok()}),
    ("get", "/_s/{code}/files", "Files", "A folder link's files", {"security": [], "parameters": [param("code"), param("t", "query")], "responses": ok()}),
    ("post", "/mcp", "Configuration", "Model Context Protocol (JSON-RPC 2.0): read-only lattice tools for external agents", {"requestBody": body({"type": "object"}), "responses": ok()}),
    ("post", "/a2a", "Configuration", "A2A (Agent2Agent) 1.0 JSON-RPC: SendMessage, GetTask, ListTasks, CancelTask; answered by the chat as the caller (a2a.enabled)", {"requestBody": body({"type": "object"}), "responses": ok()}),
    ("get", "/.well-known/agent-card.json", "Configuration", "The A2A agent card (public; 404 while a2a.enabled is off)", {"security": [], "responses": ok()}),
    ("get", "/ai/estimate", "Configuration", "Token estimates for AI tagging and video thumbnails, per files tessellation (admins)", {"responses": ok()}),
    ("post", "/{tessellation}/_ai_tag", "Configuration", "Tag existing files with the AI (admins); ?id= one file, ?all=true every file", {"parameters": [param("tessellation"), param("id", "query"), param("all", "query")], "responses": ok("Queued", status="202")}),
    ("post", "/{tessellation}/_ai_thumbnails", "Configuration", "Let the AI choose existing videos' thumbnails (admins)", {"parameters": [param("tessellation"), param("id", "query"), param("all", "query")], "responses": ok("Queued", status="202")}),
    ("post", "/join", "Server", "What another hex needs to join this lattice (admins, with password)", {"requestBody": body({"type": "object", "properties": {"password": {"type": "string"}}}), "responses": ok()}),
    ("get", "/plugins", "Server", "Plugins and their delivery state (plugins permission)", {"responses": ok()}),
    ("post", "/flush", "Server", "Flush memory to SSTables (maintenance permission)", {"responses": ok()}),
    ("post", "/compact", "Server", "Compact SSTables (maintenance permission)", {"responses": ok()}),
    ("post", "/backup", "Server", "Write a consistent backup while running (maintenance permission)", {"requestBody": body({"type": "object", "properties": {"name": {"type": "string"}}}), "responses": ok("Created", status="201")}),
    ("get", "/backups", "Server", "Backups in the backup directory (maintenance permission)", {"responses": ok()}),
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
    ("get", "/tessellations/{name}/schemas/infer", "Schemas", "A first schema inferred from up to 1,000 documents: their fields and types, nothing required", {"parameters": [NAME], "responses": ok()}),
    ("post", "/tessellations/{name}/schemas/check", "Schemas", "Check a schema without registering it", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("post", "/tessellations/{name}/schemas/rollback", "Schemas", "Roll back to an earlier version (a new version with its fields and the inverse migration)", {"parameters": [NAME], "requestBody": body({"type": "object", "required": ["to"], "properties": {"to": {"type": "integer"}, "migration": {"type": "array", "items": OBJ}}}), "responses": ok("Created", status="201")}),
    ("get", "/updates", "Server", "Versions, the newest release, and a hosted lattice's rolling update (admin; answered by the Overseer)", {"responses": ok()}),
    ("post", "/updates/rollout", "Server", "Start a rolling update of a hosted lattice to the release its hosting service offers (admin)", {"responses": ok("Accepted", status="202")}),
    ("put", "/tessellations/{name}/schemas/pending", "Schemas", "Save the next version to apply later (same body as registering; replaces any saved before)", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("delete", "/tessellations/{name}/schemas/pending", "Schemas", "Discard the version saved to apply later", {"parameters": [NAME], "responses": no_content()}),
    ("post", "/tessellations/{name}/schemas/pending/apply", "Schemas", "Apply the saved version: register it and migrate existing documents", {"parameters": [NAME], "responses": ok("Created", status="201")}),
    # Documents
    ("get", "/{tessellation}", "Documents", "List documents (?filter=&sort=&limit=&offset=&after=&fields=&total=)", {"parameters": [TESS] + [param(n, "query") for n in ("filter", "sort", "limit", "offset", "after", "fields", "total")], "responses": ok()}),
    ("post", "/{tessellation}", "Documents", "Insert a document", {"parameters": [TESS, TTL, IDEM], "requestBody": body(), "responses": ok("Created", status="201")}),
    ("get", "/{tessellation}/count", "Documents", "Count documents (?filter=)", {"parameters": [TESS, param("filter", "query")], "responses": ok()}),
    ("post", "/{tessellation}/_query", "Documents", "Query: filter, sort, paging, projection", {"parameters": [TESS], "requestBody": body({"type": "object", "properties": {"filter": OBJ, "sort": {}, "limit": {"type": "integer"}, "offset": {"type": "integer"}, "after": {"type": "string"}, "fields": {}, "total": {"type": "boolean", "description": "false skips counting every match (total is null)"}, "near": {"type": "object", "description": "similarity search: {field, vector | text, k (1-100), min_score}; the k nearest matches, best first, with _score", "properties": {"field": {"type": "string"}, "vector": {"type": "array", "items": {"type": "number"}}, "text": {"type": "string"}, "k": {"type": "integer"}, "min_score": {"type": "number"}}}}}), "responses": ok()}),
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
    ("put", "/{tessellation}/_files/{path}", "Files", "Upload a file to folder/name (raw body; a new version if the path has a file; X-HexDB-Unchanged when identical)", {"parameters": [TESS, FILE_PATH, param("modified", "query", "RFC 3339 or epoch ms"), param("created", "query", "RFC 3339 or epoch ms")], "requestBody": {"required": True, "content": {"application/octet-stream": {"schema": BINARY}}}, "responses": {**ok("Created", status="201"), **ok("A new version")}}),
    ("get", "/{tessellation}/_files/{path}", "Files", "Download a file by folder/name (Range, If-None-Match, ?version=, ?download=true)", {"parameters": [TESS, FILE_PATH, param("version", "query", schema={"type": "integer"}), param("download", "query", schema={"type": "boolean"})], "responses": FILE_CONTENT}),
    ("get", "/{tessellation}/{id}/content", "Files", "Download a file by ID (Range, If-None-Match, ?version=, ?download=true)", {"parameters": [TESS, ID, param("version", "query", schema={"type": "integer"}), param("download", "query", schema={"type": "boolean"})], "responses": FILE_CONTENT}),
    ("get", "/{tessellation}/{id}/thumbnail", "Files", "A file's thumbnail (images and Photoshop files)", {"parameters": [TESS, ID], "responses": {"200": {"description": "JPEG or PNG", "content": {"image/jpeg": {"schema": BINARY}, "image/png": {"schema": BINARY}}}}}),
    ("get", "/{tessellation}/{id}/versions", "Files", "A file's current and earlier versions", {"parameters": [TESS, ID], "responses": ok()}),
    ("post", "/{tessellation}/{id}/versions/{version}/restore", "Files", "Make an earlier version current again (as a new version)", {"parameters": [TESS, ID, param("version", schema={"type": "integer"})], "responses": ok()}),
    ("get", "/{tessellation}/_trash", "Files", "Deleted files", {"parameters": [TESS], "responses": ok("{\"documents\", \"total\"}")}),
    ("delete", "/{tessellation}/_trash", "Files", "Empty the trash", {"parameters": [TESS], "responses": ok("{\"purged\"}")}),
    ("post", "/{tessellation}/_trash/{id}/restore", "Files", "Restore a deleted file (?name=, ?path= when the original is taken)", {"parameters": [TESS, ID, param("name", "query"), param("path", "query")], "responses": ok()}),
    ("delete", "/{tessellation}/_trash/{id}", "Files", "Delete a trashed file for good", {"parameters": [TESS, ID], "responses": no_content()}),
    ("post", "/{tessellation}/_reextract", "Files", "Read metadata again (?id= for one file)", {"parameters": [TESS, param("id", "query")], "responses": ok("{\"queued\"}", status="202")}),
    ("post", "/{tessellation}/_uploads", "Files", "Start a resumable upload", {"parameters": [TESS], "requestBody": body({"type": "object", "required": ["path"], "properties": {"path": {"type": "string"}, "size": {"type": "integer"}, "content_type": {"type": "string"}, "modified": {"type": "string"}, "created": {"type": "string"}}}), "responses": ok("{\"id\", \"path\", \"offset\", \"size\", \"expires_at\"}", status="201")}),
    ("get", "/{tessellation}/_uploads/{id}", "Files", "How far a resumable upload got", {"parameters": [TESS, ID], "responses": ok()}),
    ("patch", "/{tessellation}/_uploads/{id}", "Files", "Send the next part (raw body) starting at ?offset= or Upload-Offset", {"parameters": [TESS, ID, param("offset", "query", schema={"type": "integer"}, required=False)], "requestBody": {"required": True, "content": {"application/octet-stream": {"schema": BINARY}}}, "responses": ok()}),
    ("post", "/{tessellation}/_uploads/{id}/complete", "Files", "Finish a resumable upload", {"parameters": [TESS, ID], "responses": {**ok("Created", status="201"), **ok("A new version")}}),
    ("delete", "/{tessellation}/_uploads/{id}", "Files", "Abandon a resumable upload", {"parameters": [TESS, ID], "responses": no_content()}),
    ("post", "/scrub", "Maintenance", "Verify every stored file fragment now and repair bad ones", {"responses": ok()}),
    ("post", "/transactions", "Documents", "Operations across tessellations, all or nothing", {"parameters": [IDEM], "requestBody": body(description="{\"operations\": [{\"op\": \"insert|replace|patch|delete|get|check\", \"tessellation\", \"id\", \"data\", \"if_version\"}]}"), "responses": ok()}),
    ("post", "/sql", "SQL", "Run a SQL SELECT (read-only; one page of rows, then pass next as cursor)", {"requestBody": body({"type": "object", "required": ["sql"], "properties": {"sql": {"type": "string"}, "params": {"type": "array", "description": "values for ? or $1, $2, ... placeholders"}, "page_size": {"type": "integer", "description": "rows per page, 1-10000 (default 1000)"}, "cursor": {"type": "string", "description": "next from the previous page"}}}), "responses": ok("{\"columns\": [{\"name\", \"type\"}], \"rows\": [[...]], \"next\", \"translated\"}")}),
    ("get", "/sql/tables", "SQL", "Tessellations the caller can read (a catalog for SQL clients)", {"responses": ok("{\"tables\": [{\"name\"}]}")}),
    ("get", "/sql/columns", "SQL", "A tessellation's columns: id, the current schema's fields, then fields sampled from documents", {"parameters": [param("table", "query")], "responses": ok("{\"table\", \"columns\": [{\"name\", \"type\", \"nullable\", \"source\": \"schema|sample\"}]}")}),
    ("post", "/graphql", "GraphQL", "Run a GraphQL query or mutation", {"requestBody": body({"type": "object", "properties": {"query": {"type": "string"}, "variables": OBJ, "operationName": {"type": "string"}}}), "responses": ok()}),
    # Changes
    ("get", "/changes", "Changes", "Committed changes after a sequence number (long poll with wait)", {"parameters": [param(n, "query") for n in ("after", "tessellation", "limit", "wait")], "responses": ok()}),
    ("get", "/changes/stream", "Changes", "Committed changes as Server-Sent Events", {"parameters": [param(n, "query") for n in ("after", "tessellation")], "responses": {"200": {"description": "text/event-stream"}}}),
    ("get", "/changes/ws", "Changes", "Committed changes over a WebSocket (JSON text frames)", {"parameters": [param(n, "query") for n in ("after", "tessellation")], "responses": {"101": {"description": "Switching to the WebSocket protocol"}}}),
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
    ("get", "/streams/{name}/ws", "Streams", "Messages over a WebSocket (JSON text frames)", {"parameters": [NAME] + [param(n, "query") for n in ("after", "group")], "responses": {"101": {"description": "Switching to the WebSocket protocol"}}}),
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
    ("get", "/triggers", "Triggers", "Every trigger with its run status (admins)", {"responses": ok()}),
    ("post", "/triggers", "Triggers", "Create a trigger; it runs as you (admins)", {"requestBody": body({"type": "object", "required": ["name", "tessellation", "function"], "properties": {"name": {"type": "string"}, "description": {"type": "string"}, "tessellation": {"type": "string"}, "events": {"type": "array", "items": {"enum": ["insert", "update", "delete"]}}, "timing": {"enum": ["before", "after"]}, "function": {"type": "string"}, "filter": OBJ, "enabled": {"type": "boolean"}}}), "responses": ok("Created", status="201")}),
    ("get", "/triggers/{name}", "Triggers", "A trigger with its run status", {"parameters": [NAME], "responses": ok()}),
    ("put", "/triggers/{name}", "Triggers", "Replace a trigger", {"parameters": [NAME], "requestBody": body(), "responses": ok()}),
    ("delete", "/triggers/{name}", "Triggers", "Delete a trigger", {"parameters": [NAME], "responses": {"204": {"description": "Deleted"}}}),
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

TAGS = ["Authentication", "Documents", "Tessellations", "Indexes", "Schemas", "Changes", "Streams", "Functions", "GraphQL", "SQL", "Users and roles", "Configuration", "Server"]


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
            "version": "2.2.0",
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
