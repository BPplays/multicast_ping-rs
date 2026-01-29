# multicast_ping-rs

A Rust program for testing IPv6 multicast communication with server/client modes.

## Features

- **Server Mode**: Listens on multicast address `ff12:c909:3199:e8ba:6f6f:7d23:e6ae:d85d` and responds with unicast packets
- **Client Mode**: Sends multicast requests at configurable intervals and tracks response rate

## Building

```bash
cargo build --release
```

## Usage

### Server Mode

Start the server to listen for multicast packets and respond:

```bash
cargo run -- --server
# or
cargo run -- -s
```

### Client Mode

Send multicast requests every 1000ms (default):

```bash
cargo run
```

Send requests at a custom interval (in milliseconds):

```bash
cargo run -- --interval 500
# or
cargo run -- -n 500
```

## Example Output

### Server:
```
Starting server mode...
Listening on multicast address: ff12:c909:3199:e8ba:6f6f:7d23:e6ae:d85d:9999
Server ready and listening...

Received 12 bytes from [::1]:54321: PING seq=0
Sent unicast response to [::1]:54321
```

### Client:
```
Starting client mode...
Sending multicast requests every 1000 ms
Target: ff12:c909:3199:e8ba:6f6f:7d23:e6ae:d85d:9999

Listening for responses on port 54321
Sent request #1 (seq=0)
  ✓ Received ACK (15ms)
  Stats: 1/1 received (100.0%)
```

## Requirements

- Rust 1.70 or later
- IPv6 support on your system
- Multicast-capable network interface

## Notes

- The multicast address used is `ff12:c909:3199:e8ba:6f6f:7d23:e6ae:d85d` (site-local scope)
- Default port is 9999
- Client waits up to 500ms for each response before considering it lost
