# The recorder link: two recorders, one site

Two recorders on one site are better than one — a second antenna hears what the first fades on —
but only if they are not both spending a tuner on the same control channel. The link is how they
share: a recorder that follows the control channel offers every grant it cannot record itself, and
a peer with a free tuner covering that frequency takes it.

It is deliberately the same mechanism the in-process coordinator already uses between two tuners
of one recorder. A `control` front produces grants; the coordinator routes each to whichever tuner
can record it. The link widens that set of tuners to include ones in another process or on another
box, and widens the set of control channels this recorder hears. Nothing else changes: a recorder
with `[link] enabled = false` (the default) behaves exactly as it would without it.

## Configuring it

```toml
[link]
enabled = true
bind = "0.0.0.0:47123"               # the default; UDP
peers = ["192.0.2.11:47123"]         # each recorder lists the others
key = "a-long-random-shared-secret"  # >= 16 chars, identical on every peer
hello_interval_s = 5
accept_remote_grants = true
publish_grants = true
defer_to_claims = false
grant_retries = 3                    # retransmissions of an unacknowledged grant
grant_retry_ms = 8                   # milliseconds between them
```

Per tuner:

```toml
[[device]]
id = "voice-1"
role = "voice"
remote_grants = true    # default for voice/data; false for auto/control
```

The default port is **UDP 47123**; open it between the recorders (and only between them) in any
host firewall. Generate the key with something like `openssl rand -base64 32`.

`key` is not optional. Every datagram carries a truncated HMAC-SHA256 over its whole frame, and one
that does not verify is dropped and counted; without a key the link would be a remote "tune this
recorder away" button on an open UDP port. Replay protection is keyed on what the frame
authenticates, never on the UDP source address: every frame carries its sender's random id, a frame
counter and its send time inside the HMAC. A frame sent more than 30 s from the receiver's clock is
refused (`stale` counter, and a warning), and every frame accepted inside that window is remembered
by (sender id, counter), so a captured datagram cannot be replayed — later, or from another port.
**The recorders' clocks must agree to within 30 s** (NTP; see
[operations.md](operations.md#timestamps-and-the-host-clock)), or every frame between them is
refused; a peer's grant more than 10 s old by its own timestamp is also not followed
(`grant_stale`). Keep the link on a trusted network anyway: authentication proves who sent a grant,
not that following it is a good idea.

`defer_to_claims` is off on purpose. Keyup already picks a best source among several recordings of
one call, so a second copy costs a little disk and is sometimes the better one; a call suppressed
here because a peer claimed it, and then lost there, is gone for good. Turn it on when tuner
capacity rather than disk is the constraint.

Which tuners will accept a peer's grant is `remote_grants`, whose role default is "yes" for
`voice`/`data` and "no" for `auto`/`control` — a lone `auto` front on a newly linked box takes a
peer's traffic only once that is set (in the TOML or from Keyup). `bands` on that tuner then decide
which of a peer's grants it takes ([configuration.md](configuration.md#bands--what-a-tuner-is-responsible-for)).

## Grants are acknowledged; nothing else is

A grant is the one message where loss costs a *call* rather than a stale advertisement, so it is
the one message that is retried. Every `Grant` datagram carries a frame sequence, the receiver
answers it with a `GrantAck` naming that sequence — sent straight back from the receive thread, only
once the grant has been accepted, to the address the grant came from — and the sender resends the
identical datagram every `grant_retry_ms` until the ack arrives or `grant_retries` retransmissions
are spent. With the defaults that is at most 24 ms of retrying inside the 100–300 ms a P25 grant
has before its voice starts. `grant_retries = 0` turns the retry off and still counts the
acknowledgements; `grant_retries` is capped at 8 and `grant_retry_ms` at 100, because this is
insurance against one dropped datagram, not a place to spend the grant budget.

Hello, Claim, Release and ControlState are fire-and-forget: losing one costs at most one stale
advertisement.

None of this happens on a DSP or coordinator thread. Publishing a grant is a bounded drop-oldest
channel send into the link's own sender thread (`trunkd-link-tx`), which does the framing, the
retransmissions and the give-up; a wedged sender costs grants (counted as `link.dropped`) rather
than memory or a stalled pipeline.

**A redelivered grant is a no-op.** A lost ack makes the sender resend the same datagram; the
receiver recognises it as one it already accepted (`replay` counter) so nothing is routed twice,
*and* the ack is sent again. The same frame arriving from a *different* address is a replay and is
not answered. Frames may arrive in any order.

**Old peers.** The frame's version byte is the capability flag: a peer whose frames say version 1
predates acknowledgements and is never sent one. Its grants still route and its hellos still fill
the peer table. A peer that leaves three grants in a row unacknowledged is written off — logged
once as `peer does not ack grants`, then sent grants exactly once with no retransmissions — until it
acknowledges one or a fresh `Hello` from that host puts it back on trial.

## Managing it from Keyup

Every field of `[link]` except the two retry knobs is also a **desired-state** setting: the `link`
object of the recorder-wide `recorder` block ([keyup.md](keyup.md#the-recorder-block--settings-for-the-whole-box))
carries the same eight fields, each independently managed, merged over what the box already has.
That is how an operator changes a peer list, rotates the shared key, or turns the link on across a
fleet without editing a TOML on each box. `grant_retries` and `grant_retry_ms` stay in each box's
TOML: how hard a datagram has to be chased is a property of the wire between two boxes, not of the
fleet.

| field | type | notes |
| --- | --- | --- |
| `enabled` | bool | `false` stops the link; `true` binds it. A box whose TOML has no `[link]` at all can be given one entirely from Keyup, key included. |
| `bind` | `"host:port"` | Refused unless it parses as a host and a port in `1..=65535`; names are not resolved here, so a peer that is merely down is still configurable. |
| `peers` | array of `"host:port"` | At most 16. One bad entry refuses the **whole** list rather than applying half of it. |
| `key` | string | At least 16 characters, at most 256. **Never echoed back.** |
| `hello_interval_s` | 1..=60 | Seconds between `Hello` advertisements. |
| `accept_remote_grants` | bool | Whether grants a peer heard are routed to this recorder's tuners. |
| `publish_grants` | bool | Whether grants heard here are published to peers. |
| `defer_to_claims` | bool | Whether a peer's claim suppresses recording the same call here. |

A `link` push is **merged over what is in effect**, so a push naming only `peers` keeps the bind and
the key this box already has.

The TOML block is the **boot** value; a Keyup push is the runtime override, and the TOML is never
written back. A push is applied by rebuilding the link in place — the old socket and its four
threads (receive, send/retry, ingest, hello) are stopped and new ones started inside the running
process. **No tuner restarts and no call is interrupted.** The call thread and the coordinator hold
the link *slot*, not the handle, so they route through the new one without noticing. The grant bus
and the coordinator are built for **every** run — single-tuner sites, no `[link]` in the TOML at
all — precisely so that a link Keyup turns on mid-flight has somewhere to route an arriving peer's
grant. Idle it is one sleeping thread and two empty queues. The desired state is also fetched once
*before* the first tuner is built, so the link's very first bind already uses Keyup's settings.

The key is one-way: it can be set from Keyup, and it is never echoed back or logged. What comes back
on the status POST is `key_set` and `key_fingerprint` — the first 8 hex of its SHA-256 — enough for
Keyup to show that two peers hold the *same* secret, useless to anyone who wants to speak the
protocol. The local panel displays the effective link settings and whether they came from the
config or from Keyup, and deliberately cannot edit them: one writer per setting.

## Same box, two tuners: no link at all

Two tuners of *one* trunkd process never speak the link, on loopback or otherwise. A `control` front
publishes each grant onto the in-process grant bus (a bounded drop-oldest channel), the coordinator
picks the tuner whose span and bands cover it, and the grant arrives on that tuner's own queue — no
socket, no HMAC, no serialization and nothing to acknowledge. `[link]` is for crossing a process or
a box, and nothing else.

## Same box, two processes

Two trunkd processes on one machine are peers like any other — point them at each other on loopback
with different ports:

```toml
# recorder A: the control front
[link]
enabled = true
bind = "127.0.0.1:47123"
peers = ["127.0.0.1:47124"]
key = "..."

# recorder B: the voice tuners
[link]
enabled = true
bind = "127.0.0.1:47124"
peers = ["127.0.0.1:47123"]
key = "..."
```

Give the two processes different `[server] bind` ports too, if both serve a panel.

## Checking it

`trunkd check` prints the link's state, and each tuner's `remote_grants`:

```sh
trunkd --config /etc/trunkd/trunkd.toml check
#   link enabled bind=0.0.0.0:47123 peers=1 [192.0.2.11:47123]
#     tuner 'ctrl' remote_grants=false
#     tuner 'voice-1' remote_grants=true
```

The local panel and the Keyup status POST both carry a `link` block (the live one: peers, their
claims, and the counters), and the panel additionally shows the settings in force under `recorder`:

```sh
curl -s localhost:8080/api/status | jq .link
curl -s localhost:8080/api/status | jq .recorder      # effective settings + where each came from
```

```json
{
  "enabled": true,
  "instance": "rec-a",
  "bind": "0.0.0.0:47123",
  "peers": [
    {
      "instance": "rec-b",
      "addr": "192.0.2.11:47123",
      "alive": true,
      "last_seen_s": 2,
      "last_hello_s": 2,
      "sites": [
        {
          "system": "Example County", "site": "Site 1", "nac": 291,
          "control_hz": 851200000, "locked": true, "tsbk_rate": 15,
          "tuners": [
            { "key": "voice-1", "role": "voice",
              "center_hz": 856000000, "span_hz": 8000000,
              "bands": [[851000000, 866000000]] }
          ]
        }
      ],
      "grants_rx": 412,
      "grants_tx": 380,
      "grants_sent": 380, "grants_acked": 379,
      "grants_retried": 4, "grants_unacked": 1,
      "acks_grants": true,
      "claims": [
        { "site": "Example County/Site 1", "frequency_hz": 852137500,
          "talkgroup": 1301, "tuner": "voice-1", "age_s": 1 }
      ]
    }
  ],
  "counters": {
    "rx": 9124, "tx": 8807, "dropped": 0,
    "auth_failed": 0, "replay": 0,
    "grants_tx": 380, "grants_rx": 412, "grant_dup": 37,
    "grant_deferred": 0, "grant_site_unknown": 0,
    "claims_tx": 355, "claims_rx": 349,
    "grants_retried": 4, "grants_unacked": 1,
    "grant_acks_tx": 412, "grant_acks_rx": 379
  }
}
```

The five per-peer grant fields are the ack path: `grants_sent` is grants handed to that peer,
`grants_acked` the ones it answered, `grants_retried` the retransmissions spent on it,
`grants_unacked` the grants it never answered at all, and `acks_grants` is `true`, `false` (written
off as an old or silent build) or `null` when no grant has resolved yet. They are matched to the
peer by *host*: a peer's datagrams arrive from its ephemeral send port while the counters are kept
against the address this recorder sends to.

`last_seen_s` is the age of the last datagram of any kind from that peer; `last_hello_s` is the age
of its last **hello**, the periodic keepalive and so the one that answers "is the other end still
there". It is `null` for a peer known only from its traffic.

What the counters mean when something is wrong:

| counter | what a rising number says |
| --- | --- |
| `auth_failed` | a peer has a different `key`, or something else is on the port |
| `replay` | duplicated datagrams (a bridged network), or an actual replay |
| `dropped` | the send buffer or the ingest queue is full; the link is shedding |
| `grant_site_unknown` | a peer serves a site this recorder does not, or the labels disagree and neither side has a NAC lock yet |
| `grant_dup` | normal on two recorders hearing one control channel: the peer's grant matched one already published here within two seconds |
| `grant_deferred` | `defer_to_claims` is on and a peer already had the call |
| `grants_retried` | datagrams are being lost between here and a peer — a few is the retry doing its job; a number close to `grants_tx` is a network to look at |
| `grants_unacked` | grants a peer never acknowledged: it is on an older build, it is down, or the loss is bad enough that four datagrams in a row went missing |

Sites are matched on `system`/`site` labels first and on NAC second, so two operators who name a
site differently still route to each other once both are locked.
