#!/bin/sh
# Checks that every list of the running demo has data.
# Usage: demo/verify.sh [base URL]
set -u
BASE="${1:-http://localhost:8088}"
TOKEN="demo-admin-token"
failed=0

check() {
  path="$1"
  filter="$2"
  body=$(curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE$path") || { echo "FAIL $path (request)"; failed=1; return; }
  count=$(printf '%s' "$body" | python3 -c "import json,sys; d=json.load(sys.stdin); print($filter)" 2>/dev/null)
  if [ -z "$count" ] || [ "$count" = "0" ]; then
    echo "FAIL $path: empty"
    failed=1
  else
    echo "ok   $path: $count"
  fi
}

total='d["total"]'
length='len(d)'
check /me 'd["name"]'
check /organizations "$length"
check /connected/sources "$length"
check /inbox "$total"
check /mail/threads "$total"
check /calendar/events "$total"
check /calendar/suggestions "$length"
check /issues "$total"
check /pulls "$total"
check /deals "$total"
check '/deals?org=acme' "$total"
check '/deals?org=globex' "$total"
check /bank/accounts "$total"
check /bank/transactions "$total"
check /invoices "$total"
check /contracts "$total"
check /rooms "$total"
check /holdings "$total"
check /holdings/quotes "$total"
check /numbers 'len(d["currencies"])'
check /tasks "$length"
check /rules "$length"
check /memory "$length"
check /alerts "$length"
check /ready 'd["total"]'
check /posts "$length"
check '/posts?status=published' "$length"
check /posts/pillars "$length"
check /posts/calendar "$length"
check /decks "$length"
check /contracts/templates "$length"
check /automations "$length"
check /agents "$length"
check /talk/threads "$length"
check /runs "$length"
check /briefs/latest 'd["kind"]'
check /recaps "$length"
check /approvals "$length"

exit $failed
