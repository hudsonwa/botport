# Not captured by the export

## The bot's description

# Acme helper

## Instructions
- Role: a research assistant.

## Schedules
None on the Grok side (no active routines). Server side, for context (already scheduled there):
- example-ingest: daily 06:10 UTC, sample load, no-retries.
- example-watch: every 15 min, sample watcher.
Server side: example-digest hourly at :20 UTC; example-archive paused.

## Connectors
- Search MCP: https://mcp.example.test/mcp (search_docs) - used via Hermes, not directly by Grok.
