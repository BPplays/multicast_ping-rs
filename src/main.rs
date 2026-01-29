use clap::Parser;
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{self, ErrorKind};
use std::mem::MaybeUninit;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::{Duration, Instant};

const MULTICAST_ADDR: &str = "ff12:c909:3199:e8ba:6f6f:7d23:e6ae:d85d";
const PORT: u16 = 9999;
const BUFFER_SIZE: usize = 1024;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
	/// Server mode - listen for multicast and respond with unicast
	#[arg(short, long)]
	server: bool,

	/// Interval in milliseconds between client requests (client mode only)
	#[arg(short, long, default_value_t = 1000)]
	interval: u64,
}

fn main() -> io::Result<()> {
	let args = Args::parse();

	if args.server {
		run_server()?;
	} else {
		run_client(args.interval)?;
	}

	Ok(())
}

fn run_server() -> io::Result<()> {
	println!("Starting server mode...");
	println!("Listening on multicast address: {}:{}", MULTICAST_ADDR, PORT);

	// Create UDP socket
	let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
	socket.set_reuse_address(true)?;

	// Bind to the port on all interfaces
	let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, PORT, 0, 0);
	socket.bind(&bind_addr.into())?;

	// Join the multicast group
	let multicast_addr: Ipv6Addr = MULTICAST_ADDR.parse().unwrap();
	socket.join_multicast_v6(&multicast_addr, 0)?;

	println!("Server ready and listening...\n");

	let mut buffer = [MaybeUninit::<u8>::uninit(); BUFFER_SIZE];

	loop {
		// Receive multicast message
		match socket.recv_from(&mut buffer) {
			Ok((len, sender_addr)) => {
				// Safe to assume_init because recv_from initialized these bytes
				let received_data: Vec<u8> = buffer[..len]
					.iter()
					.map(|b| unsafe { b.assume_init() })
					.collect();
				let received = String::from_utf8_lossy(&received_data);
				println!("Received {} bytes from {}: {}", len, sender_addr.as_socket().unwrap(), received);

				// Send unicast response back to the sender
				let response = b"ACK";
				match socket.send_to(response, &sender_addr) {
					Ok(_) => println!("Sent unicast response to {}\n", sender_addr.as_socket().unwrap()),
					Err(e) => eprintln!("Failed to send response: {}\n", e),
				}
			}
			Err(e) => {
				eprintln!("Error receiving: {}", e);
			}
		}
	}
}

fn run_client(interval_ms: u64) -> io::Result<()> {
	println!("Starting client mode...");
	println!("Sending multicast requests every {} ms", interval_ms);
	println!("Target: {}:{}\n", MULTICAST_ADDR, PORT);

	// Create UDP socket for sending
	let send_socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;

	// Bind to any available port for sending
	let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
	send_socket.bind(&bind_addr.into())?;

	// Set multicast interface (0 = default)
	send_socket.set_multicast_if_v6(0)?;

	// Get the local port we're bound to for receiving responses
	let local_addr = send_socket.local_addr()?;
	let local_port = local_addr.as_socket().unwrap().port();
	println!("Listening for responses on port {}", local_port);

	// Set socket to non-blocking for receiving
	send_socket.set_nonblocking(true)?;

	let multicast_addr: Ipv6Addr = MULTICAST_ADDR.parse().unwrap();
	let dest_addr = SocketAddrV6::new(multicast_addr, PORT, 0, 0);

	let mut sent_count = 0u64;
	let mut received_count = 0u64;
	let mut sequence = 0u64;

	let interval = Duration::from_millis(interval_ms);
	let response_timeout = Duration::from_millis(500); // Wait up to 500ms for response

	loop {
		let send_time = Instant::now();

		// Send multicast request
		let message = format!("PING seq={}", sequence);
		match send_socket.send_to(message.as_bytes(), &dest_addr.into()) {
			Ok(_) => {
				sent_count += 1;
				println!("Sent request #{} (seq={})", sent_count, sequence);
			}
			Err(e) => {
				eprintln!("Failed to send: {}", e);
			}
		}

		// Wait for response with timeout
		let mut buffer = [MaybeUninit::<u8>::uninit(); BUFFER_SIZE];
		let mut received_response = false;

		while send_time.elapsed() < response_timeout {
			match send_socket.recv_from(&mut buffer) {
				Ok((len, _sender)) => {
					// Safe to assume_init because recv_from initialized these bytes
					let received_data: Vec<u8> = buffer[..len]
						.iter()
						.map(|b| unsafe { b.assume_init() })
						.collect();
					let response = String::from_utf8_lossy(&received_data);
					if response.starts_with("ACK") {
						received_count += 1;
						received_response = true;
						println!("  ✓ Received ACK ({}ms)", send_time.elapsed().as_millis());
						break;
					}
				}
				Err(e) if e.kind() == ErrorKind::WouldBlock => {
					// No data available yet, sleep briefly
					std::thread::sleep(Duration::from_millis(10));
				}
				Err(e) => {
					eprintln!("  Error receiving: {}", e);
					break;
				}
			}
		}

		if !received_response {
			println!("  ✗ No response received");
		}

		// Calculate and display statistics
		let success_rate = if sent_count > 0 {
			(received_count as f64 / sent_count as f64) * 100.0
		} else {
			0.0
		};

		println!("  Stats: {}/{} received ({:.1}%)\n", received_count, sent_count, success_rate);

		sequence += 1;

		// Wait for the next interval
		let elapsed = send_time.elapsed();
		if elapsed < interval {
			std::thread::sleep(interval - elapsed);
		}
	}
}
