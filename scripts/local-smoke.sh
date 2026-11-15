#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(solana-test-validator --version)" != "solana-test-validator 3.1.14 "* ]]; then
    echo "Put Agave 3.1.14 on PATH before running this smoke test." >&2
    exit 1
fi
mode="${1:-program}"
case "$mode" in
    program|timeout|rfq|app|load|drain|recovery|race|admin) ;;
    *) echo "Usage: bash scripts/local-smoke.sh [program|timeout|rfq|app|load|drain|recovery|race|admin]" >&2; exit 1 ;;
esac

run_dir="$(mktemp -d "${TMPDIR:-/tmp}/stryk-smoke.XXXXXXXX")"
if [[ -n "${STRYK_PG_LIB_DIR:-}" ]]; then
    export DYLD_FALLBACK_LIBRARY_PATH="$STRYK_PG_LIB_DIR"
fi
rpc_port="${STRYK_SMOKE_PORT:-18899}"
faucet_port="$((rpc_port + 10))"
program="76CMRtnnJCAA9udEy9Rdq1G6PcxLpbouZWYWxoqtE6VP"
receiver="B7s3eyFtLSijY7bAx99kREdfpMK55MCr5r77j837TmCs"
solana-keygen new --no-bip39-passphrase --silent --outfile "$run_dir/admin.json" >/dev/null
admin="$(solana-keygen pubkey "$run_dir/admin.json")"

if [[ -n "${STRYK_PLATFORM_TOOLS:-}" ]]; then
    platform_tools="$(cd "$STRYK_PLATFORM_TOOLS" && pwd)"
    RUSTC="$platform_tools/rust/bin/rustc" \
        CARGO_TARGET_SBPF_SOLANA_SOLANA_LINKER="$platform_tools/llvm/bin/ld.lld" \
        "$platform_tools/rust/bin/cargo" build --locked --release --target sbpf-solana-solana \
        -p stryk -p stryk-fixture-receiver
    deploy_dir="target/sbpf-solana-solana/release"
else
    cargo build-sbf --tools-version v1.52 --manifest-path programs/stryk/Cargo.toml -- --locked
    cargo build-sbf --tools-version v1.52 --manifest-path tests/fixture-receiver/Cargo.toml -- --locked
    deploy_dir="target/deploy"
fi
if [[ "$mode" == "rfq" || "$mode" == "app" || "$mode" == "load" || "$mode" == "drain" || "$mode" == "recovery" ]]; then
    cargo build --locked -p stryk-cli -p stryk-service
else
    cargo build --locked -p stryk-cli
fi

rpc="http://127.0.0.1:$rpc_port"
solana-test-validator --ledger "$run_dir/ledger" --rpc-port "$rpc_port" \
    --faucet-port "$faucet_port" --gossip-port "$((rpc_port + 20))" \
    --dynamic-port-range "$((rpc_port + 21))-$((rpc_port + 100))" --mint "$admin" \
    --upgradeable-program "$program" "$deploy_dir/stryk.so" "$admin" \
    --bpf-program "$receiver" "$deploy_dir/stryk_fixture_receiver.so" \
    --quiet >"$run_dir/validator.log" 2>&1 &
validator_pid="$!"
pids=("$validator_pid")
cleanup() {
    kill -CONT "$validator_pid" 2>/dev/null || true
    if [[ -n "${service_container:-}" ]]; then
        docker rm -f "$service_container" >/dev/null 2>&1 || true
        rm -f "$run_dir/container-payer.json"
    fi
    kill "${pids[@]}" 2>/dev/null || true
    wait "${pids[@]}" 2>/dev/null || true
    if [[ -f "$run_dir/db/postmaster.pid" ]]; then
        pg_ctl -D "$run_dir/db" -m fast stop >/dev/null 2>&1 || true
    fi
    if [[ -n "${database_container:-}" ]]; then
        docker rm -f "$database_container" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

start_app() {
    local pgport="$((rpc_port + 5))"
    local database="postgres://stryk@127.0.0.1:$pgport/postgres"
    if [[ "$mode" == "load" && -z "${STRYK_SMOKE_POSTGRES_IMAGE:-}" ]]; then
        database=""
    elif [[ -n "${STRYK_SMOKE_POSTGRES_IMAGE:-}" ]]; then
        database_container="$(docker run --rm -d -e POSTGRES_USER=stryk -e POSTGRES_PASSWORD=stryk-smoke \
            -p "127.0.0.1:$pgport:5432" "$STRYK_SMOKE_POSTGRES_IMAGE")"
        database="postgres://stryk:stryk-smoke@127.0.0.1:$pgport/postgres"
        for _ in {1..20}; do
            docker exec "$database_container" pg_isready -h 127.0.0.1 -U stryk >/dev/null 2>&1 && break
            sleep 1
        done
    else
        command -v initdb >/dev/null || { echo "Put PostgreSQL on PATH for the RFQ smoke test." >&2; return 1; }
        local args=(-D "$run_dir/db" -U stryk --auth=trust --no-locale)
        if [[ -n "${STRYK_PG_SHARE_DIR:-}" ]]; then args+=(-L "$STRYK_PG_SHARE_DIR"); fi
        if ! initdb "${args[@]}" >"$run_dir/postgres.log" 2>&1; then
            cat "$run_dir/postgres.log" >&2
            return 1
        fi
        pg_ctl -D "$run_dir/db" -l "$run_dir/postgres.log" -o "-h 127.0.0.1 -p $pgport" -w start >/dev/null
    fi
    local expiry_seconds=300
    if [[ "$mode" == "rfq" || "$mode" == "recovery" ]]; then expiry_seconds=55; fi
    if [[ "$mode" == "drain" ]]; then expiry_seconds=65; fi
    if [[ "$mode" == "app" ]]; then expiry_seconds="${STRYK_SMOKE_EXPIRY_SECONDS:-300}"; fi
    target/debug/stryk-cli --rpc "$rpc" demo --admin "$run_dir/admin.json" --directory "$run_dir" \
        --expiry-seconds "$expiry_seconds" >"$run_dir/demo.json"
    market="$(json_field market "$run_dir/demo.json")"
    domain="$(json_field domain "$run_dir/demo.json")"
    expiry="$(json_field expiry "$run_dir/demo.json")"
    maker_b="$(json_field maker_b "$run_dir/demo.json")"
    proof="$(json_field proof "$run_dir/demo.json")"
    listen="127.0.0.1:$((rpc_port + 4))"
    service="http://$listen"
    options=(--rpc "$rpc" --listen "$listen" --market "$market" --fixture-spot 150000000)
    if [[ -n "$database" ]]; then
        options+=(--database-url "$database")
        if [[ "$mode" != "load" ]]; then
            options+=(--settlement-payer "$run_dir/admin.json")
            if [[ "$mode" != "app" || "${STRYK_SMOKE_NO_FIX:-}" != "1" ]]; then
                options+=(--fixture-proof "$proof")
            fi
        fi
    fi
    if [[ "$mode" == "app" ]]; then
        options+=(--web-dir target/web)
    fi
    start_service
    start_makers
}

start_service() {
    local log_filter="${RUST_LOG:-stryk_service=info}"
    if [[ "$mode" == "recovery" ]]; then log_filter="${log_filter},stryk_service::settlement=info"; fi
    if [[ "$mode" == "rfq" && -n "${STRYK_SMOKE_SERVICE_IMAGE:-}" ]]; then
        service_container="stryk-smoke-${run_dir##*.}"
        # The disposable key is readable by the image's unprivileged UID. The host temp directory is private.
        cp "$run_dir/admin.json" "$run_dir/container-payer.json"
        chmod 0444 "$run_dir/container-payer.json"
        docker run --rm -d --network host --name "$service_container" -e RUST_LOG="stryk_service=info" \
            --mount "type=bind,source=$run_dir/container-payer.json,target=$run_dir/admin.json,readonly" \
            "$STRYK_SMOKE_SERVICE_IMAGE" "${options[@]}" >/dev/null
    else
        RUST_LOG="$log_filter" target/debug/stryk-service "${options[@]}" >>"$run_dir/service.log" 2>&1 &
        service_pid="$!"
        pids+=("$service_pid")
    fi
    for _ in {1..20}; do
        curl -fsS "$service/health" >/dev/null 2>&1 && break
        if [[ -n "${service_container:-}" ]]; then
            docker inspect --format '{{.State.Running}}' "$service_container" 2>/dev/null | rg -qx true || break
        elif ! kill -0 "$service_pid" 2>/dev/null; then
            cat "$run_dir/service.log" >&2
            return 1
        fi
        sleep 1
    done
    if ! curl -fsS "$service/health" >/dev/null; then
        if [[ -n "${service_container:-}" ]]; then docker logs "$service_container" >&2 || true; fi
        return 1
    fi
}

start_makers() {
    local maker premium
    for maker in a b c; do
        premium=120
        if [[ "$maker" == "b" ]]; then premium=190; fi
        if [[ "$maker" == "c" ]]; then premium=160; fi
        target/debug/stryk-cli maker --wallet "$run_dir/maker-$maker.json" --service "$service" \
            --market "$market" --domain "$domain" --premium-bps "$premium" >"$run_dir/maker-$maker.log" 2>&1 &
        pids+=("$!")
    done
    for _ in {1..30}; do
        if rg -q subscribed "$run_dir/maker-a.log" && rg -q subscribed "$run_dir/maker-b.log" && rg -q subscribed "$run_dir/maker-c.log" \
            && curl -fsS "$service/v1/markets/$market/expiries" | rg -q "$expiry"; then return 0; fi
        sleep 1
    done
    cat "$run_dir"/maker-?.log >&2
    return 1
}

take() {
    local kind="$1"
    target/debug/stryk-cli --rpc "$rpc" take --wallet "$run_dir/writer.json" \
        --service "$service" --expiry "$expiry" --kind "$kind" --strike 150000000 --quantity 1000000000 \
        >"$run_dir/fill-$kind.json"
    [[ "$(json_field maker "$run_dir/fill-$kind.json")" == "$maker_b" ]]
    [[ "$(json_field premium "$run_dir/fill-$kind.json")" == "2850000" ]]
    cat "$run_dir/fill-$kind.json"
}

load_smoke() {
    start_app
    local writer
    writer="$(json_field writer "$run_dir/demo.json")"
    local load_args=(load --service "$service" --expiry "$expiry" --writer "$writer" --winner "$maker_b")
    local sizes="$run_dir/rss-kib.txt"
    (
        while kill -0 "$service_pid" 2>/dev/null; do
            ps -o rss= -p "$service_pid"
            sleep 1
        done
    ) >"$sizes" &
    pids+=("$!")
    target/debug/stryk-cli "${load_args[@]}" | tee "$run_dir/load.json"
    target/debug/stryk-cli "${load_args[@]}" --rate 400 --overload | tee "$run_dir/overload.json"
    curl -fsS "$service/metrics" >"$run_dir/metrics.txt"
    rg -Fx 'stryk_rfq_quoted_total 2280' "$run_dir/metrics.txt"
    rg -Fx 'stryk_rfq_rejected_capacity_total 2720' "$run_dir/metrics.txt"
    rg -Fx 'stryk_maker_subscriptions 3' "$run_dir/metrics.txt"
    awk 'NR==1{base=$1} $1>peak{peak=$1} END{print "service RSS KiB baseline=" base " peak=" peak}' "$sizes"
}

drain_smoke() {
    start_app
    local writer
    writer="$(json_field writer "$run_dir/demo.json")"
    local -a fills=()
    local i pid
    for ((i=0; i<32; i++)); do
        target/debug/stryk-cli --rpc "$rpc" take --wallet "$run_dir/writer.json" \
            --service "$service" --expiry "$expiry" --kind "$((i % 2))" \
            --strike 150000000 --quantity 100000000 >"$run_dir/drain-$i.json" &
        fills+=("$!")
        pids+=("$!")
    done
    for pid in "${fills[@]}"; do wait "$pid"; done
    python3 scripts/measure-drain.py "$rpc" "$service" "$run_dir" "$writer" "$maker_b" | tee "$run_dir/drain.json"
}

rfq_smoke() {
    start_app
    if [[ "$mode" == "rfq" ]]; then
        target/debug/stryk-cli maker --wallet "$run_dir/maker-a.json" --service "$service" \
            --market "$market" --domain "$domain" --lane 1 --premium-bps 900 \
            >"$run_dir/maker-unregistered-lane.log" 2>&1 &
        pids+=("$!")
        for _ in {1..15}; do
            rg -q subscribed "$run_dir/maker-unregistered-lane.log" && break
            sleep 1
        done
        rg -q subscribed "$run_dir/maker-unregistered-lane.log"
    fi
    take 0
    if [[ "$mode" == "recovery" ]]; then
        restart_during_rfq
    fi
    take 1
    if [[ "$mode" == "rfq" ]]; then
        for _ in {1..45}; do
            if [[ "$(rg -c '"opened_signature":' "$run_dir/maker-b.log" || true)" == 2 ]]; then
                break
            fi
            sleep 1
        done
        [[ "$(rg -c '"opened_signature":' "$run_dir/maker-b.log")" == 2 ]]
        ! rg -q '"opened_signature":' "$run_dir/maker-a.log" "$run_dir/maker-c.log"
        [[ "$(rg -c 'reason="insufficient_inventory_or_cap"' "$run_dir/maker-unregistered-lane.log")" == 2 ]]
        echo "Only the winning maker received both finalized fill notices."
        echo "The higher bid from an unregistered lane was rejected for both fills."
    fi
    if [[ "$mode" == "recovery" ]]; then
        kill -STOP "$validator_pid"
        for _ in {1..15}; do
            curl -fsS "$service/health" >/dev/null
            curl -fsS "$service/metrics" >"$run_dir/outage-metrics.txt"
            if rg -qx 'stryk_receipt_scan_healthy 0' "$run_dir/outage-metrics.txt" \
                && rg -qx 'stryk_settlement_scan_healthy 0' "$run_dir/outage-metrics.txt"; then break; fi
            sleep 1
        done
        rg -Fx 'stryk_receipt_scan_healthy 0' "$run_dir/outage-metrics.txt"
        rg -Fx 'stryk_settlement_scan_healthy 0' "$run_dir/outage-metrics.txt"
        kill -CONT "$validator_pid"
        restart_after_settlement_submission
    fi
    local writer="$(json_field writer "$run_dir/demo.json")"
    for _ in {1..90}; do
        if ! curl -fsS "$service/v1/positions?owner=$writer" >"$run_dir/portfolio.json"; then
            sleep 1
            continue
        fi
        if python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    row = json.load(f)
assert len(row["positions"]) == 2 and row["income_atoms"] == "5700000"
assert all(position["opened_signature"] and position["settled_signature"] and position["outcome"] == 2 for position in row["positions"])
for position in row["positions"]:
    spot = position["entry_spot"]
    assert spot["price"] == "150000000"
    assert 0 <= position["opened_at"] - spot["observed_at"] <= 45
' "$run_dir/portfolio.json" 2>/dev/null; then
            echo "Postgres recovered both finalized fills and worker settlements, with 5.7 USDC in total premium."
            curl -fsS "$service/metrics" >"$run_dir/settled-metrics.txt"
            if [[ -n "${service_container:-}" ]]; then
                curl -fsS "$service/earn/sol?strategy=put&strike=149000000" | rg -F '<title>Stryk</title>'
                curl -fsS "$service/wasm/stryk_web_bg.wasm" -o "$run_dir/release.wasm"
                python3 -c 'import sys; assert open(sys.argv[1],"rb").read(4) == b"\x00asm"' "$run_dir/release.wasm"
                [[ "$(docker exec "$service_container" id -u)" == 65532 ]]
                echo "The release image served a ticket deep link and WASM and settled both strategies as UID 65532."
            fi
            if [[ "$mode" == "recovery" && -n "${database_container:-}" ]]; then
                verify_rebuild "$writer"
            fi
            return 0
        fi
        sleep 1
    done
    cat "$run_dir/portfolio.json" >&2
    if [[ -n "${service_container:-}" ]]; then docker logs "$service_container" >&2 || true
    else cat "$run_dir/service.log" >&2
    fi
    return 1
}

verify_rebuild() {
    local writer="$1" original_database rebuilt_database index
    docker exec "$database_container" createdb -U stryk stryk_rebuild
    for ((index=0; index < ${#options[@]}; index++)); do
        if [[ "${options[$index]}" == "--database-url" ]]; then
            original_database="${options[$((index + 1))]}"
            rebuilt_database="${original_database%/*}/stryk_rebuild"
            options[$((index + 1))]="$rebuilt_database"
            break
        fi
    done
    crash_and_restart
    for _ in {1..60}; do
        curl -fsS "$service/v1/positions?owner=$writer" >"$run_dir/rebuilt-portfolio.json"
        if python3 - "$run_dir/portfolio.json" "$run_dir/rebuilt-portfolio.json" <<'PY'
import json, sys
def stable(path, *, chain_only):
    with open(path) as file:
        data = json.load(file)
    data.pop("as_of_slot")
    data.pop("indexing_healthy", None)
    for position in data["positions"]:
        position.pop("as_of_slot")
        entry = position.pop("entry_spot", None)
        assert (entry is None) == chain_only
    return data
sys.exit(stable(sys.argv[1], chain_only=False) != stable(sys.argv[2], chain_only=True))
PY
        then
            echo "A fresh database rebuilt the same positions, signatures and income from finalized chain state."
            echo "A chain-only rebuild has no historical RFQ spot observations."
            return 0
        fi
        sleep 1
    done
    cat "$run_dir/rebuilt-portfolio.json" "$run_dir/service.log" >&2
    return 1
}

restart_during_rfq() {
    local writer request_id state
    writer="$(json_field writer "$run_dir/demo.json")"
    curl -fsS -H 'Content-Type: application/json' -d "{\"version\":1,\"market\":\"$market\",\"expiry\":\"$expiry\",\"writer\":\"$writer\",\"kind\":1,\"strike\":\"150000000\",\"quantity\":\"1000000000\"}" \
        "$service/v1/rfqs" >"$run_dir/interrupted-rfq.json"
    request_id="$(json_field id "$run_dir/interrupted-rfq.json")"
    crash_and_restart
    state="$(curl -sS -o "$run_dir/restarted-rfq.json" -w '%{http_code}' "$service/v1/rfqs/$request_id")"
    [[ "$state" == "404" && "$(json_field code "$run_dir/restarted-rfq.json")" == "unknown_rfq" ]]
    start_makers
}

restart_after_settlement_submission() {
    for _ in {1..300}; do
        if rg -q 'settlement submitted' "$run_dir/service.log"; then
            crash_and_restart
            return
        fi
        sleep .25
    done
    echo "Worker never logged a settlement submission" >&2
    return 1
}

crash_and_restart() {
    kill -KILL "$service_pid"
    wait "$service_pid" 2>/dev/null || true
    start_service
}

json_field() {
    python3 -c 'import json, sys; print(json.load(open(sys.argv[2]))[sys.argv[1]])' "$1" "$2"
}

admin_smoke() {
    local cash domain maker expiry_time market expiry lane amount
    local admin_cli=(target/debug/stryk-cli --rpc "$rpc" admin --wallet "$run_dir/admin.json")
    spl-token --url "$rpc" --fee-payer "$run_dir/admin.json" --output json \
        create-token --mint-authority "$admin" --decimals 6 >"$run_dir/quote-mint.json"
    cash="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["commandOutput"]["address"])' "$run_dir/quote-mint.json")"
    solana-keygen new --no-bip39-passphrase --silent --outfile "$run_dir/admin-maker.json" >/dev/null
    maker="$(solana-keygen pubkey "$run_dir/admin-maker.json")"
    domain="$maker"
    if "${admin_cli[@]}" initialize-market --quote-mint "$cash" --receiver SysvarRent111111111111111111111111111111111 --domain "$domain" >"$run_dir/admin-invalid-receiver.log" 2>&1; then
        echo "Market was initialized with a non-executable receiver" >&2
        return 1
    fi
    rg -q 'InstructionError' "$run_dir/admin-invalid-receiver.log"
    "${admin_cli[@]}" initialize-market --quote-mint "$cash" --receiver "$receiver" --domain "$domain" >"$run_dir/admin-market.json"
    market="$(json_field address "$run_dir/admin-market.json")"
    expiry_time="$(python3 -c 'import time; now=int(time.time()); print(now - (now - 115200) % 604800 + 604800)')"
    if "${admin_cli[@]}" create-expiry --market "$market" --expires-at "$((expiry_time + 1))" >"$run_dir/admin-reject.log" 2>&1; then
        echo "Weekly market accepted a misaligned expiry" >&2
        return 1
    fi
    rg -q 'InstructionError' "$run_dir/admin-reject.log"
    "${admin_cli[@]}" create-expiry --market "$market" --expires-at "$expiry_time" >"$run_dir/admin-expiry.json"
    expiry="$(json_field address "$run_dir/admin-expiry.json")"
    solana --url "$rpc" --output json account "$expiry" >"$run_dir/admin-expiry-account.json"
    python3 - "$run_dir/admin-expiry-account.json" "$expiry_time" <<'PY'
import base64, json, sys
with open(sys.argv[1]) as file:
    data = base64.b64decode(json.load(file)["account"]["data"][0])
expires_at = int.from_bytes(data[65:73], "little", signed=True)
deadline = int.from_bytes(data[73:81], "little", signed=True)
assert expires_at == int(sys.argv[2]) and deadline == expires_at + 86_400
PY
    "${admin_cli[@]}" register-lane --market "$market" --maker "$maker" --lane 3 >"$run_dir/admin-lane.json"
    lane="$(json_field address "$run_dir/admin-lane.json")"
    amount=3000000000
    "${admin_cli[@]}" set-capacity --market "$market" --lane "$lane" --expiry "$expiry" --limit "$amount" >"$run_dir/admin-capacity.json"
    "${admin_cli[@]}" set-lane-enabled --market "$market" --lane "$lane" --enabled false >"$run_dir/admin-disabled.json"
    "${admin_cli[@]}" set-lane-enabled --market "$market" --lane "$lane" --enabled true >"$run_dir/admin-enabled.json"
    "${admin_cli[@]}" pause-market --market "$market" --paused true >"$run_dir/admin-paused.json"
    "${admin_cli[@]}" pause-market --market "$market" --paused false >"$run_dir/admin-resumed.json"
    target/debug/stryk-cli --rpc "$rpc" inspect --maker "$maker" --market "$market" --expiry "$expiry" --lane 3 >"$run_dir/admin-inspect.json"
    [[ "$(json_field cap_limit_atoms "$run_dir/admin-inspect.json")" == "$amount" ]]
    [[ "$(json_field cap_filled_atoms "$run_dir/admin-inspect.json")" == "0" ]]
    echo "The admin CLI rejected a non-executable receiver, created a weekly market with a 24-hour fix window, rejected a misaligned expiry, registered a maker, set its cap, and exercised pause and lane controls."
}

for _ in {1..30}; do
    if solana cluster-version --url "$rpc" >/dev/null 2>&1; then
        if [[ "$mode" == "rfq" || "$mode" == "recovery" ]]; then
            rfq_smoke
        elif [[ "$mode" == "load" ]]; then
            load_smoke
        elif [[ "$mode" == "drain" ]]; then
            drain_smoke
        elif [[ "$mode" == "admin" ]]; then
            admin_smoke
        elif [[ "$mode" == "app" ]]; then
            if [[ ! -f target/web/wasm/stryk_web.js ]]; then
                echo "Run bash scripts/build-web.sh first." >&2
                exit 1
            fi
            start_app
            echo "Stryk is available at $service using $rpc."
            echo "Local fixture keys and configuration are in $run_dir."
            wait "$service_pid"
        else
            flags=(--rpc "$rpc" smoke --admin "$run_dir/admin.json" --fixture-receiver "$receiver")
            if [[ "$mode" == "timeout" ]]; then flags+=(--timeout); fi
            if [[ "$mode" == "race" ]]; then flags+=(--race); fi
            target/debug/stryk-cli "${flags[@]}"
        fi
        echo "Local ledger and validator log are in $run_dir"
        exit 0
    fi
    kill -0 "$validator_pid" 2>/dev/null || break
    sleep 1
done
cat "$run_dir/validator.log" >&2
if [[ -f "$run_dir/ledger/validator.log" ]]; then
    tail -30 "$run_dir/ledger/validator.log" >&2
fi
exit 1
