# ACP schema fixture

`schema.v1.json` is the official Agent Client Protocol v1 JSON Schema (schema crate 1.28.0, 2026-10-30), copied unchanged from
<https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json> (Apache-2.0).

`tests/acp_schema.rs` validates every message the adapter sends to the (fake) agent against it, so field names and shapes come
from the schema and not from memory. To update: replace the file with the new `schema/v1/schema.json`, run
`cargo test -p anycon-adapters --test acp_schema`, and fix what the new schema rejects.
