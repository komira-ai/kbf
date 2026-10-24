# The scheduler routes: leases and queue

Part of [the operator API](api.md).


What the scheduler holds and what waits for it. Reads, open like `GET /v1/nodes`, and
unredacted. The state is in memory: after a restart both are empty until the daemons
reconnect, and a frontend lists only what its own scheduler holds. Every answer is `Cache-Control: no-store`; durations carry the
unit in the key and times are milliseconds since the Unix epoch.

**`GET /v1/leases`**: the leases held now, in lease order.

```json
{
  "term": "1792000000123456",
  "items": [
    {
      "lease": "1792000000123456.7", "node": "node-a", "phase": "running",
      "age_ms": 41200, "since_ms": 40100,
      "granted_unix_ms": 1792000100000, "started_unix_ms": 1792000101100,
      "operation": "op-12", "instance": "main", "digest": "<hash>/<size>",
      "platform": "arch=x86_64,os=linux", "qos": "ci"
    }
  ],
  "next_page": null
}
```

- `term`: the scheduler's leader term, a string (a 64-bit number a JSON reader would
  round). Every lease's id is `term.seq`, and a restart starts a new term.
- `phase`: `proposed` (the grant is not committed, no `Start` sent), `granted` (sent, the
  worker has not said it started) or `running`. `age_ms` is since the grant, `since_ms`
  since the phase began. `started_unix_ms` is `null` until the worker said it started.
- `platform`: the action's platform properties as `key=value`, comma-separated, or `any`.
- Query: `node` keeps one node's leases (a node the scheduler does not know has none:
  an empty list, `200`); `page_size` is 1 to 200 (default 100); `page_token` is the token
  inside the previous answer's `next_page`, which is the root-relative link of the next
  page (`null` on the last) and is bound to the `node` it was made for. An unknown or
  repeated key, an empty or over-long `node`, an out-of-range `page_size` and a token
  made for another filter are `400`.

**`GET /v1/queue`**: the operations waiting for a lease, summarised. It takes no query.

```json
{
  "depth": 4, "oldest_age_ms": 61500,
  "by_platform": [{ "platform": "os=macos", "qos": "ci", "depth": 4, "oldest_age_ms": 61500 }],
  "waiting": [{
    "reason": "none of the 2 live workers satisfies the action's platform; the closest, node-a, lacks os=macos",
    "count": 4, "example_operation": "op-3", "example_instance": "main",
    "example_digest": "<hash>/<size>"
  }],
  "avoided": [{ "operation": "op-9", "instance": "main", "digest": "<hash>/<size>", "nodes": ["node-b"] }],
  "avoided_total": 1
}
```

- `depth` is the length of the scheduler's queue, as `queued_operations` of the frontend
  drain status; `oldest_age_ms` is `null` when nothing waits. Ages count from when the
  operation last entered the queue (its submission, or the give-up of its lease).
- `by_platform`: one entry per platform and QoS level, platform first, the more urgent
  level first.
- `waiting`: operations no live worker can run, one entry per reason; the example is the
  first of them in queue order. Operations that wait only for room are in `depth` and
  `by_platform` and in no `waiting` entry.
- `avoided`: queued operations placement keeps off some nodes (a run ended there as the
  farm's fault, or the node dropped its `Start`), in operation order, at most 100;
  `avoided_total` counts them all.

