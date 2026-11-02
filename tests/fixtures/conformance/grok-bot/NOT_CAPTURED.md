# Not captured by the export

## The bot's description

### Schedules
- **Nightly tidy**: `0 2 * * *` America/Chicago — prompt: "Tidy the visitor log"
- **Greet visitor**: on an event — prompt: "Greet the new visitor"

### Connectors
- **visitor-log**: MCP server at https://visitors.example.test/mcp (tools: list_visitors)
- **Web search**: built-in

### Credentials
- `VISITOR_LOG_TOKEN`
