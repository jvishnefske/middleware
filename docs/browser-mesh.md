# Browser mesh: a room of browsers

Two or more browsers that open the web page and join the same room link
directly to each other (WebRTC data channels), and their payloads'
channels connect by name. design.md "Browser mesh (BM…)" has the rules;
this page is how to use it and how to run a signalling server.

## Use it

1. Open the page (`cd web && npm run dev`) in two browsers, or two
   profiles of one. Two tabs of the same profile share one node id and
   cannot both join.
2. On the Editor tab, in the Room panel, type the same room name in both
   (`a-z`, `0-9`, `_`) and press Join. Each panel lists the other node.
3. Compile and simulate a payload in each. A channel one payload writes
   and the other reads under the same name is connected: its key is
   `room/<room>/<name>`.

A payload built with the Rust SDK declares its channels itself. A WAT or
AssemblyScript payload from the editor has no `mw.channels` section; type
its channels under "Server and channels", for example `audio:in level:out`
(they are declared as byte channels, none required). The payload still
has to open them with `channel_open`.

A board connected on the Device tab publishes into the room. Nothing from
the room is sent to the board.

## What a room is not

Peers are not authenticated. Anyone who knows the room's name can join it
and publish under any node id. A room carries channel data only: system
keys never cross it, and nothing in it can start, stop, install or bind a
payload. Do not put data in a room that must be trusted or kept private
from someone who can guess its name.

Browsers behind a symmetric NAT need a TURN server to reach each other;
none is configured. A room is a full mesh up to seventeen pages; beyond
that some pages are not linked to each other.

## The signalling server

Any PeerJS-compatible server works. The default is the public PeerJS
server (`0.peerjs.com`, port 443, key `peerjs`). It does not list its
peers, so pages find each other through a rendezvous: the first page in
the room registers the id `mw-<room>-hub` and tells the others who is
there.

To run your own (not a dependency of this repository):

```
npx peerjs --port 9000 --allow_discovery
```

and in the Room panel set the server to `localhost`, port `9000`, path
`/`, key `peerjs`, TLS off. With `--allow_discovery` the pages read the
server's peer list instead of using a rendezvous. Without the flag the
rendezvous is used, as with the public server.

The server only introduces peers to each other (offers, answers, ICE
candidates). Channel data goes between the browsers.
