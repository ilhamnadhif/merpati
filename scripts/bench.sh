#!/usr/bin/env bash
# Reproducible benchmarks for the PRD section 2 targets (macOS).
#
#   scripts/bench.sh            # builds release binaries, prints a report
#
# Measures: binary sizes, cold start to first frame, idle memory (RSS and
# physical footprint as shown by Activity Monitor), 5 open request tabs,
# a 100 MB response, a 10,000-request workspace, and runner memory over
# 100/1,000/3,000 iterations. The GUI is driven by the MERPATI_AUTOMATION hook.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${BENCH_DIR:-$(mktemp -d -t merpati-bench)}"
# BENCH_PROFILE=dist measures the shipped profile; BENCH_TARGET_DIR another target dir.
PROFILE="${BENCH_PROFILE:-release}"
TARGET="${BENCH_TARGET_DIR:-$ROOT/target}"
APP="$TARGET/$PROFILE/merpati-app"
CLI="$TARGET/$PROFILE/merpati"
export MERPATI_SECRETS=file

echo "== building $PROFILE binaries"
(cd "$ROOT" && cargo build --profile "$PROFILE" --target-dir "$TARGET" -p merpati-app -p merpati-cli >/dev/null)
printf "binary sizes: app %.1f MB, cli %.1f MB\n" \
  "$(echo "$(stat -f%z "$APP") / 1048576" | bc -l)" "$(echo "$(stat -f%z "$CLI") / 1048576" | bc -l)"

echo "== preparing fixtures in $WORK"
mkdir -p "$WORK/www"
python3 - "$WORK" <<'PY'
import json, sys, os
work = sys.argv[1]
with open(os.path.join(work, "www/big.json"), "w") as f:
    f.write("[\n"); i = 0
    while f.tell() < 100 * 1024 * 1024:
        f.write(json.dumps({"id": i, "name": f"user-{i}", "email": f"u{i}@example.com", "bio": "lorem ipsum " * 8}) + ",\n"); i += 1
    f.write('{"end": true}\n]\n')
items = [{"name": f"Folder {a}", "item": [{"name": f"Request {a}-{b}", "request": {"method": "GET", "url": {"raw": "{{baseUrl}}/items/%d/%d" % (a, b)}},
          "event": [{"listen": "test", "script": {"exec": ["pm.test('ok', () => pm.response.to.have.status(200));"]}}]} for b in range(100)]} for a in range(100)]
json.dump({"info": {"name": "Big", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"}, "item": items,
           "variable": [{"key": "baseUrl", "value": "http://127.0.0.1:8765"}]}, open(os.path.join(work, "big.json"), "w"))
small = {"info": {"name": "Small", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
         "item": [{"name": f"R{i}", "request": {"method": "GET", "url": {"raw": "{{baseUrl}}/www-index-%d" % i}},
                   "event": [{"listen": "test", "script": {"exec": ["pm.test('responds', () => pm.expect(pm.response.code).to.be.a('number'));"]}}]} for i in range(5)],
         "variable": [{"key": "baseUrl", "value": "http://127.0.0.1:8765"}]}
json.dump(small, open(os.path.join(work, "small.json"), "w"))
PY
rm -rf "$WORK/ws-empty" "$WORK/ws-big" "$WORK/ws-small"
"$CLI" init "$WORK/ws-empty" --name Empty >/dev/null
"$CLI" init "$WORK/ws-big" --name Big >/dev/null
"$CLI" init "$WORK/ws-small" --name Small >/dev/null
start=$(python3 -c 'import time; print(time.time())')
"$CLI" import "$WORK/big.json" -w "$WORK/ws-big" >/dev/null
echo "import of 10,000 requests: $(python3 -c "import time; print(round(time.time()-$start, 2))") s"
"$CLI" import "$WORK/small.json" -w "$WORK/ws-small" >/dev/null

python3 -m http.server 8765 --bind 127.0.0.1 --directory "$WORK/www" >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT
sleep 1

run_app() { # steps.json workspace
  MERPATI_CONFIG_DIR="$WORK/config-$(basename "$2")" MERPATI_AUTOMATION="$1" "$APP" "$2" >"$WORK/app.log" 2>&1 &
  local pid=$! worker_max=0 r
  while kill -0 "$pid" 2>/dev/null; do
    r=$(ps -axo rss=,command= | awk '/--merpati-script-worker/ && !/awk/ {s+=$1} END {print s+0}')
    [ "$r" -gt "$worker_max" ] && worker_max=$r
    sleep 0.1
  done
  wait "$pid" || true
  grep 'automation: memory' "$WORK/app.log" || true
  [ "$worker_max" -gt 0 ] && printf "  script worker process: %.1f MB max RSS\n" "$(echo "$worker_max / 1024" | bc -l)"
  return 0
}

cat > "$WORK/idle.json" <<J
[{"report_memory": "first frame"}, {"wait_ms": 3000}, {"report_memory": "idle"},
 {"url": "http://127.0.0.1:8765/big.json"}, {"send": true}, {"wait_idle": true}, {"wait_ms": 2000},
 {"report_memory": "100 MB response shown"}, {"quit": true}]
J
cat > "$WORK/tabs.json" <<J
[{"open": "R0"}, {"send": true}, {"wait_idle": true}, {"open": "R1"}, {"send": true}, {"wait_idle": true},
 {"open": "R2"}, {"send": true}, {"wait_idle": true}, {"open": "R3"}, {"send": true}, {"wait_idle": true},
 {"open": "R4"}, {"send": true}, {"wait_idle": true}, {"wait_ms": 1500}, {"report_memory": "5 request tabs"}, {"quit": true}]
J
cat > "$WORK/big.json.steps" <<J
[{"report_memory": "10k workspace first frame"}, {"expand": "Big"}, {"wait_ms": 800}, {"report_memory": "10k tree expanded"}, {"quit": true}]
J

echo "== GUI (memory = RSS and physical footprint)"
run_app "$WORK/idle.json" "$WORK/ws-empty"
run_app "$WORK/tabs.json" "$WORK/ws-small"
run_app "$WORK/big.json.steps" "$WORK/ws-big"

echo "== Collection Runner memory (scripts run in a worker process)"
for n in 100 1000 3000; do
  /usr/bin/time -l "$CLI" run "$WORK/ws-small" -n "$n" --silent --no-keychain >/dev/null 2>"$WORK/time-$n.txt" &
  pid=$!
  worker_max=0
  cli_max=0
  while kill -0 "$pid" 2>/dev/null; do
    r=$(ps -axo rss=,command= | awk '/--merpati-script-worker/ && !/awk/ {s+=$1} END {print s+0}')
    [ "$r" -gt "$worker_max" ] && worker_max=$r
    r=$(ps -axo rss=,command= | awk -v cli="$CLI run" 'index($0, cli) && !/script-worker|awk|time -l/ {s+=$1} END {print s+0}')
    [ "$r" -gt "$cli_max" ] && cli_max=$r
    sleep 0.1
  done
  wait "$pid" || true
  printf "  %5d iterations: CLI %.1f MB, script worker %.1f MB (max RSS, sampled), %.2f ms per request\n" "$n" \
    "$(echo "$cli_max / 1024" | bc -l)" \
    "$(echo "$worker_max / 1024" | bc -l)" \
    "$(echo "$(grep ' real ' "$WORK/time-$n.txt" | awk '{print $1}') * 1000 / ($n * 5)" | bc -l)"
done
