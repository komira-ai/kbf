# The client box's gateway

A build client's REAPI calls must reach the farm's current leader server, and the leader
can move. A box that runs build clients runs two processes for this: `kbf-gateway` and
Envoy. Servers register with the gateway; the gateway tells Envoy, over xDS, which one
leads; Envoy routes every REAPI call there.

```text
kbf-server (each) --Register, mutual TLS--> kbf-gateway --xDS (ADS), loopback--> Envoy <-- build clients
                                                                                    |
                                                          REAPI, health-checked ----+--> the leader
```

## Running it

```text
kbf-gateway serve \
  --register-listen 0.0.0.0:8990 \
  --tls-cert gateway.pem --tls-key gateway.key --client-ca farm-ca.pem \
  [--xds-listen 127.0.0.1:18000] [--ttl-ms 6000]

kbf-gateway envoy-config \
  [--listen 127.0.0.1:8980] [--xds 127.0.0.1:18000] [--admin 127.0.0.1:9901] \
  [--upstream-ca farm-ca.pem --upstream-san-suffix .farm.example] > envoy.json
envoy --config-path envoy.json
```

`serve` prints `kbf-gateway <version> register=<addr> xds=<addr>` once both listeners are
bound and stops on SIGINT or SIGTERM. `--xds-listen` must be a loopback address: xDS is
plain text and unauthenticated. `envoy-config` prints the bootstrap; its bytes depend only
on the flags.

Servers register with `kbf.gateway.v1.Registration/Register` on `--register-listen`.

## Certificates

Registration is mutual TLS against the farm CA, and only server certificates register. A
server certificate allows serverAuth and clientAuth (a server presents its certificate as
the client certificate of its calls) and names exactly one DNS name, which
is the node name the gateway knows the server by. A daemon's certificate (clientAuth only)
is refused PERMISSION_DENIED. With `--upstream-ca`, Envoy accepts a server only if its
certificate chains to that CA and has a DNS name ending in `--upstream-san-suffix`.

## What the gateway decides

- Servers are expected to call about every two seconds. An entry that is not renewed expires after
  `--ttl-ms` (three missed calls by default). Nothing is stored: a restarted gateway
  rebuilds its state from the next calls.
- The leader is the live entry that claims the role with the highest term. At equal
  terms the lowest node name wins, and the gateway logs the tie.
- Envoy is sent one endpoint, the leader's address, or none while no server leads (calls
  then fail UNAVAILABLE). A change is pushed to Envoy at once. If Envoy refuses a response
  (NACK), the gateway logs why and does not send it again.
- Envoy health-checks the endpoint with `grpc.health.v1`, which answers with `/readyz`'s
  checks. A server that lost the role but still holds a fresh claim fails that check and
  is not sent traffic.
