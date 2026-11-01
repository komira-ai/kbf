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
- `whole_node`: present only when the server runs with `--whole-node-max`; with the option
  off the document has none of it. Shown as:

  ```json
  "whole_node": {
    "held": 0, "marks": 1, "whole": 1, "draining": 1, "ceiling": 3,
    "max_whole_node": 4, "capable_hosts": 4, "keep_for_ordinary": 1, "max_draining": 1,
    "waiting": { "early": 0, "holding": 1, "barred": 0, "cap_ceiling": 0,
      "cap_keep_for_ordinary": 0, "cap_max_draining": 0, "cap_per_consumer": 0,
      "last_capable": 0, "queued": 1, "unservable": 0 },
    "requests": [{
      "operation": "op-12", "instance": "main", "digest": "<hash>/<size>",
      "state": "holding", "reason": "holding", "may_hold_in_ms": null,
      "worker": "node-a", "held_for_ms": 61000, "deadline_in_ms": 100000,
      "give_ups": 1, "draining": true, "barred": [], "unservable": null
    }],
    "requests_total": 2
  }
  ```

  `held` (workers held by a whole-machine lease), `marks` (held by a queued request while
  they drain), `whole` (their sum), `draining` (marked workers that still have leases),
  `ceiling` (the most that may be whole now) and the three settings are the scheduler's
  `whole_summary`. `waiting` counts the queued whole-machine requests by `reason`, every
  reason present. `requests` lists them in queue order, at most 100 of `requests_total`.
  `state` is `early` (before `--whole-node-drain-after-secs`: `may_hold_in_ms`), `holding`
  (`worker`, `held_for_ms`, `deadline_in_ms` or `null` when the hold is never given up,
  `give_ups`, `draining`: whether the worker still has leases), `barred` (`give_ups` and
  `barred`, each worker with `until_in_ms`, `null` when the bar has no end), `stopped`, `queued` (nothing is recorded
  against it: it waits for a worker; the UI says "Waiting") or `unservable` (`unservable`
  is the scheduler's text, free-form, shown as it is and not to be matched on). `reason`
  is the `waiting` key: the state's name, and for `stopped` the cap (`cap_ceiling`,
  `cap_keep_for_ordinary`, `cap_max_draining`, `cap_per_consumer`) or `last_capable`.
  Times are milliseconds from the answer; fields a state does not have are `null`, and
  `barred` is empty. The marks and waits are as of the last placement round.

