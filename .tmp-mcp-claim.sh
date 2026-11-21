#!/bin/bash
# Fleet VM-seat claim via MCP-HTTP (board MCP down; mirrors reviewer's path).
TOK=$(tr -d ' \r' < /root/fleet/bay-controller/.polymrc | grep -o '"session_token":"[^"]*"' | cut -d'"' -f4)
sid_file=/tmp/bay-claim-sid.txt
hdr_file=/tmp/bay-claim-hdr.txt
curl -sD "$hdr_file" -o /dev/null -m 8 -X POST http://localhost:3030/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Authorization: Bearer $TOK" \
  --data-binary @/tmp/bay-init.json
SID=$(grep -i '^mcp-session-id:' "$hdr_file" | tr -d '\r' | cut -d' ' -f2)
echo "SID=$SID" | tee "$sid_file"
curl -s -o /dev/null -m 8 -X POST http://localhost:3030/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Authorization: Bearer $TOK" \
  -H "mcp-session-id: $SID" \
  --data-binary @/tmp/bay-notif.json
curl -s -m 20 -X POST http://localhost:3030/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Authorization: Bearer $TOK" \
  -H "mcp-session-id: $SID" \
  --data-binary @/tmp/bay-claim.json
