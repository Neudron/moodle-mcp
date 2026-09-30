#!/bin/sh
# Fake open-browser-control: newline-delimited MCP JSON-RPC over stdio.
png="$1"; log="$2"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*)
      r='{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}' ;;
    *'"name":"browser_screenshot"'*)
      echo "browser_screenshot" >> "$log"
      case "$line" in
        *'"selector"'*) if [ -n "$FAKE_MCP_FAIL_SELECTOR" ]; then
          printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":true,"content":[]}}\n' "$id"; continue; fi ;;
      esac
      printf '\211PNG\r\n\032\n' > "$png"
      r="{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"screenshot_path\\\":\\\"$png\\\"}\"}]}" ;;
    *'"name":"browser_get_dom"'*)
      echo "browser_get_dom" >> "$log"
      r="{\"content\":[{\"type\":\"text\",\"text\":\"$FAKE_MCP_DOM\"}]}" ;;
    *'"name":"'*)
      n=$(printf '%s' "$line" | sed 's/.*"name":"\([a-z_]*\)".*/\1/')
      echo "$n" >> "$log"
      r='{"content":[]}' ;;
    *) r='{}' ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$r"
done
