import base64
import json
import math
import sys
import time
from pathlib import Path
from urllib.request import Request, urlopen

rpc_url, service, directory, writer, maker = sys.argv[1:]


def fetch(url, body=None):
    request = Request(url, body, {"Content-Type": "application/json"})
    with urlopen(request, timeout=5) as response:
        return json.load(response)


def rpc(method, *params):
    body = json.dumps(dict(jsonrpc="2.0", id=1, method=method, params=params))
    return fetch(rpc_url, body.encode())["result"]


fills = [json.loads(path.read_text()) for path in Path(directory).glob("drain-*.json")]
assert len(fills) == 32
assert all(fill["maker"] == maker and fill["premium"] == "285000" for fill in fills)
positions = {fill["position"]: fill["fill"] for fill in fills}
assert len(positions) == len(fills)

deadline = time.monotonic() + 155
while True:
    history = fetch(f"{service}/v1/positions?owner={writer}&history=true")
    rows = history["positions"]
    if len(rows) == len(fills) and all(
        row["opened_signature"] and row["settled_signature"] for row in rows
    ):
        break
    assert time.monotonic() < deadline, f"timed out with {len(rows)} terminal positions"
    time.sleep(1)

assert history["income_atoms"] == str(len(fills) * 285_000)
assert all(row["outcome"] == 2 for row in rows)
assert {row["address"]: row["opened_signature"] for row in rows} == positions
assert len({row["settled_signature"] for row in rows}) == len(rows)
assert {kind: sum(row["kind"] == kind for row in rows) for kind in (0, 1)} == {0: 16, 1: 16}

samples = []
for row in rows:
    assert row["expires_at"] > row["opened_at"]
    signature = row["settled_signature"]
    transaction = rpc(
        "getTransaction", signature,
        {"commitment": "finalized", "encoding": "base64", "maxSupportedTransactionVersion": 0},
    )
    assert transaction["meta"]["err"] is None
    samples.append({
        "position": row["address"],
        "fill_signature": row["opened_signature"],
        "signature": signature,
        "slot": transaction["slot"],
        "settled_at": row["settled_at"],
        "bytes": len(base64.b64decode(transaction["transaction"][0], validate=True)),
        "fee_lamports": transaction["meta"]["fee"],
        "units": transaction["meta"]["computeUnitsConsumed"],
    })

expiry = rows[0]["expires_at"]
lags = sorted(sample["settled_at"] - expiry for sample in samples)
assert lags[0] >= 0
print(json.dumps({
    "positions": len(rows),
    "writer_wallets": 1,
    "maker_wallets": 1,
    "execution_after_expiry_seconds": {
        "first": lags[0],
        "p95": lags[math.ceil(0.95 * len(lags)) - 1],
        "last": lags[-1],
    },
    "positions_per_second_from_expiry": round(len(lags) / max(1, lags[-1]), 2),
    "execution_slots": len({sample["slot"] for sample in samples}),
    "income_atoms": history["income_atoms"],
    "settlements": samples,
}, indent=2))
