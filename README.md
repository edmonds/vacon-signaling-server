# vacon-signaling-server

This is a WebSocket signaling server for the [Vacon video conferencing tool](https://github.com/edmonds/vacon).

The Vacon client connects to the signaling server's WebSocket endpoint `/api/v1/offer-answer?{SessionID}` and waits to receive the session start indicator, a 1-byte message with the value 0. Once a second client has connected to the same session, the signaling server will send this session start indicator to the first client, which will kick off the [SDP offer/answer exchange](https://datatracker.ietf.org/doc/html/rfc3264). The Vacon signaling server then only needs to transparently forward messages between the two clients connected to the session and ensure that only two clients can join the same session.

# Installing

Run `cargo build --release`.

Install the `vacon-signaling-server` binary into `/usr/local/bin`.

# Configuration

`vacon-signaling-server` can listen on several different socket types (TCP, Unix, systemd socket activation). A reverse proxy such as nginx should be deployed in front of `vacon-signaling-server` to provide TLS termination so that Vacon clients can connect using a `wss://` (WebSocket Secure) URI.

See the example systemd units [examples/vacon-signaling-server.service](examples/vacon-signaling-server.service) and [examples/vacon-signaling-server.socket](examples/vacon-signaling-server.socket). These units will set up `vacon-signaling-server` for systemd socket activation on the Unix socket path `/run/vacon-signaling-server.websocket`. The `--exit-on-idle` command-line option will be enabled in order to automatically shut down the server after a period of inactivity. The corresponding nginx example site config [examples/nginx-site.conf](examples/nginx-site.conf) will proxy WebSocket connections to the Unix socket.

# License

Copyright (c) 2024 The Vacon Authors

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 2 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with this program. If not, see [https://www.gnu.org/licenses/](https://www.gnu.org/licenses/).
