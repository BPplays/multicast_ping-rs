use clap::Parser;
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{self, ErrorKind};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MULTICAST_ADDR: Ipv6Addr = Ipv6Addr::new(0xff12, 0xc09, 0x3199, 0xe8ba, 0x6f6f, 0x7d23, 0xe6ae, 0xd85d);
const PORT: u16 = 9999;
const BUFFER_SIZE: usize = 1024;

#[derive(Parser, Debug)]
#[command(author, version, about = "IPv6 Multicast Client/Server Monitor", long_about = None)]
struct Args {
	/// Run in server mode (listen for multicast and respond with unicast)
	#[arg(short, long)]
	server: bool,

	/// Interval in milliseconds between multicast requests (client mode)
	#[arg(short = 'n', long, default_value = "1000")]
	interval: u64,

	/// Network interface name (e.g., eth0, wlan0, Ethernet)
	#[arg(short = 'i', long)]
	interface: Option<String>,
}

fn main() -> io::Result<()> {
	let args = Args::parse();

	if args.server {
		println!("Starting server mode...");
		println!("Listening on multicast address: [{}]:{}", MULTICAST_ADDR, PORT);
		run_server(args.interface)?;
	} else {
		println!("Starting client mode...");
		println!("Sending multicast requests every {} ms", args.interval);
		println!("Target: [{}]:{}", MULTICAST_ADDR, PORT);
		run_client(args.interval, args.interface)?;
	}

	Ok(())
}

fn run_server(interface: Option<String>) -> io::Result<()> {
	// Create UDP socket
	let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;

	// Allow address reuse
	socket.set_reuse_address(true)?;
	#[cfg(unix)]
	socket.set_reuse_port(true)?;

	// Bind to the multicast port
	let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, PORT, 0, 0);
	socket.bind(&bind_addr.into())?;

	// Join the multicast group
	let interface_index = if let Some(ref if_name) = interface {
		get_interface_index(if_name)?
	} else {
		0 // Default interface
	};

	socket.join_multicast_v6(&MULTICAST_ADDR, interface_index)?;
	println!("Joined multicast group on interface index {}", interface_index);

	let mut buf = [0u8; BUFFER_SIZE];
	let mut packet_count = 0u64;

	loop {
		match socket.recv_from(&mut buf) {
			Ok((len, addr)) => {
				packet_count += 1;
				let client_addr = match addr.as_socket() {
					Some(socket_addr) => socket_addr,
					None => {
						eprintln!("Invalid address format");
						continue;
					}
				};

				println!("[{}] Received {} bytes from {}", packet_count, len, client_addr);

				// Send unicast response back to the client
				let response = format!("RESPONSE:{}", packet_count);
				match socket.send_to(response.as_bytes(), &addr) {
					Ok(_) => println!("[{}] Sent response to {}", packet_count, client_addr),
					Err(e) => eprintln!("[{}] Failed to send response: {}", packet_count, e),
				}
			}
			Err(e) => {
				eprintln!("Error receiving: {}", e);
			}
		}
	}
}

fn run_client(interval_ms: u64, interface: Option<String>) -> io::Result<()> {
	// Create UDP socket for sending
	let send_socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;

	// Bind to any available port for sending
	let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
	send_socket.bind(&bind_addr.into())?;

	// Set multicast interface if specified
	let interface_index = if let Some(ref if_name) = interface {
		let idx = get_interface_index(if_name)?;
		send_socket.set_multicast_if_v6(idx)?;
		println!("Using interface index {} for multicast", idx);
		idx
	} else {
		0
	};

	// Create socket for receiving responses
	let recv_socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
	recv_socket.set_reuse_address(true)?;

	// Get the local port we're bound to
	let local_addr = send_socket.local_addr()?;
	let local_port = match local_addr.as_socket() {
		Some(SocketAddr::V6(addr)) => addr.port(),
		Some(SocketAddr::V4(_)) => {
			return Err(io::Error::new(ErrorKind::Other, "Expected IPv6 address"));
		}
		None => {
			return Err(io::Error::new(ErrorKind::Other, "Invalid local address"));
		}
	};

	// Bind receive socket to the same port
	let recv_bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, local_port, 0, 0);
	recv_socket.bind(&recv_bind_addr.into())?;

	// Set receive timeout
	recv_socket.set_read_timeout(Some(Duration::from_millis(interval_ms / 2)))?;

	println!("Bound to local port: {}", local_port);

	let sent_count = Arc::new(AtomicU64::new(0));
	let recv_count = Arc::new(AtomicU64::new(0));

	let recv_count_clone = Arc::clone(&recv_count);
	let sent_count_clone = Arc::clone(&sent_count);

	// Spawn receiver thread
	let recv_handle = std::thread::spawn(move || {
		let mut buf = [0u8; BUFFER_SIZE];
		loop {
			match recv_socket.recv_from(&mut buf) {
				Ok((len, addr)) => {
					let count = recv_count_clone.fetch_add(1, Ordering::SeqCst) + 1;
					let msg = String::from_utf8_lossy(&buf[..len]);
					if let Some(socket_addr) = addr.as_socket() {
						println!("← Received response #{} from {}: {}", count, socket_addr, msg);
					}
				}
				Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
					// Timeout is expected, continue
				}
				Err(e) => {
					eprintln!("Receive error: {}", e);
				}
			}
		}
	});

	// Main sending loop
	let multicast_target = SocketAddrV6::new(MULTICAST_ADDR, PORT, 0, 0);
	let interval = Duration::from_millis(interval_ms);
	let start_time = Instant::now();

	loop {
		let count = sent_count.fetch_add(1, Ordering::SeqCst) + 1;
		let message = format!("PING:{}", count);

		match send_socket.send_to(message.as_bytes(), &multicast_target.into()) {
			Ok(_) => {
				let received = recv_count.load(Ordering::SeqCst);
				let percent = if count > 0 {
					(received as f64 / count as f64) * 100.0
				} else {
					0.0
				};

				let elapsed = start_time.elapsed().as_secs();
				println!("→ Sent multicast #{} | Responses: {}/{} ({:.1}%) | Runtime: {}s",
						 count, received, count, percent, elapsed);
			}
			Err(e) => {
				eprintln!("Send error: {}", e);
			}
		}

		std::thread::sleep(interval);
	}
}

fn get_interface_index(if_name: &str) -> io::Result<u32> {
	#[cfg(unix)]
	{
		use std::ffi::CString;
		let c_name = CString::new(if_name)
			.map_err(|_| io::Error::new(ErrorKind::InvalidInput, "Invalid interface name"))?;

		let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
		if index == 0 {
			return Err(io::Error::new(
				ErrorKind::NotFound,
				format!("Interface '{}' not found", if_name),
			));
		}
		Ok(index)
	}

	#[cfg(windows)]
	{
		use std::net::UdpSocket;
		// On Windows, try to get the interface index
		// This is a simplified approach - on Windows you may need to use
		// the Windows API (GetAdaptersAddresses) for better interface lookup

		// Try to parse as a number first (Windows sometimes uses indices directly)
		if let Ok(index) = if_name.parse::<u32>() {
			return Ok(index);
		}

		// Otherwise, return error with helpful message
		Err(io::Error::new(
			ErrorKind::NotFound,
			format!(
				"Interface '{}' lookup not fully supported on Windows. \
				Try using the interface index number instead (find with 'netsh interface ipv6 show interface')",
				if_name
			),
		))
	}
}
