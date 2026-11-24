# Design: customer-managed keys

Status: implemented in 2.7 (`hexdb_core/src/kms.rs`, `sigv4.rs`; tests in `hexdb_tests/tests/managed_keys.rs`). Decisions 2026-11-23: AWS KMS and Azure Key Vault; envelope encryption, rotation, revocation.

## Why

Every hex encrypts what it stores (SSTables, WAL records, file content, metadata) with its storage key, `storage.encryption_key`, 32 bytes in the configuration or the environment. Whoever has the configuration has the data. A customer who wants to keep control of their data, and to be able to cut HexDB off from it, wants that key to be usable only through a key service they own.

## Envelope encryption

- The **data key** stays what it is today: 32 random bytes, the key ring's current key, used exactly as now. Nothing on disk changes format.
- It's stored only **wrapped** by the customer's **key-encryption key** in AWS KMS or Azure Key Vault. The configuration holds the wrapped bytes and the KMS key's identifier, never the data key:

```toml
[storage.key]
provider = "aws_kms"          # or "azure_key_vault"
key_id = "arn:aws:kms:us-east-1:111122223333:key/1234abcd-..."   # or https://<vault>.vault.azure.net/keys/<name>/<version>
wrapped = "base64:..."        # the data key, wrapped by key_id
check_minutes = 15            # how often the hex asks the key service again (0: only at start)
```

- At start the hex asks the key service to unwrap it (AWS `Decrypt`, Azure `unwrapKey`) and keeps the data key in memory only. `storage.encryption_key` must then be empty: one or the other.
- `storage.previous_encryption_keys` still works for data key rotation, and each previous key can be a wrapped one (`"wrapped:aws_kms:<key_id>:base64:..."`), unwrapped the same way.

Credentials for the key service come from the environment, as the providers' own tools read them: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally `AWS_SESSION_TOKEN` (region from the key ARN); `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET` (a service principal with the Key Vault Crypto User role, or `unwrapKey`/`wrapKey` permissions). Requests are signed with SigV4 (AWS) or carry an Entra ID token from the client-credentials flow (Azure), on the HTTP and hashing libraries HexDB already has.

## Revocation

- **At start:** if the key service refuses to unwrap (the key is disabled, scheduled for deletion or deleted, or the hex's credentials lost permission), the hex doesn't start and says why. A network failure is retried for up to a minute first.
- **While running:** every `check_minutes` the hex unwraps again. A definite refusal (AWS `DisabledException`, `KMSInvalidStateException`, `NotFoundException`, `AccessDeniedException`; Azure 403 or a disabled key) is audited as `storage.key_revoked`, alerts administrators on every alert channel, and makes the hex flush what it holds (it still has the key in memory) and shut down. It then can't start until the key is usable again. A network failure is only a warning: the key service being unreachable for a while doesn't take the lattice down.
- Each unwrap is audited on the hex (`storage.key_unwrapped`); the key service records it too (CloudTrail, Azure Monitor).

## Rotation

- **The key-encryption key:** AWS rotates key material behind the same key ID, and old wraps keep working; Azure creates a new key version. `hexdb keys rewrap --key-id <new>` unwraps the data key with the old KMS key and wraps it with the new one, printing the new `wrapped` value. No data is rewritten.
- **The data key:** as today: a new data key becomes current, the old one moves to `previous_encryption_keys`, and compaction rewrites data under the new one. `hexdb keys new --provider ... --key-id ...` makes a new data key already wrapped.

## Commands

- `hexdb keys new --provider aws_kms|azure_key_vault --key-id <id>`: a new random data key, wrapped; prints the `[storage.key]` section.
- `hexdb keys wrap --provider ... --key-id <id>`: wraps the configuration's current plain `storage.encryption_key` (moving an existing hex to a customer-managed key; then remove the plain key).
- `hexdb keys rewrap --key-id <new>`: rewraps under another KMS key.
- `hexdb keys check`: unwraps once and reports, without printing the key.

## In a lattice

Each hex unwraps its own copy. Hexes of one lattice share the data key (as they share `storage.encryption_key` today), so they share the wrapped value and the KMS key. Revoking the KMS key stops every hex within `check_minutes`.

## Testing

- SigV4 for the `kms` service against AWS's published Signature Version 4 test suite.
- A mock AWS KMS and a mock Azure Key Vault (and Entra ID token endpoint) in the tests: start, unwrap, a revocation mid-run (flush, shutdown, refusal to start), a network failure (keeps running), rewrap.
