use anyhow::{bail, Context, Result};
use clap::Parser;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(author, version, about = "Simple IPv6 multicast client/server that pings multicast and reports reply %")]
struct Args {
	/// server mode (listen for multicast and reply unicast)
	#[arg(short = 's', long = "server")]
	server: bool,

	/// multicast IPv6 address to use (required)
	#[arg(short = 'g', long = "group", default_value = "ff12:c:909:3199:e8ba:6f6f:7d23:e6ae:d85d")]
	group: String,

	/// UDP port to use
	#[arg(short = 'p', long = "port", default_value_t = 12345)]
	port: u16,

	/// interval in milliseconds between client multicast requests (client mode)
	#[arg(short = 'n', long = "interval", default_value_t = 1000)]
	interval_ms: u64,

	/// interface to use (name like "eth0" or numeric index). optional.
	#[arg(short = 'I', long = "iface")]
	iface: Option<String>,

	/// message to send (client mode) or reply with (server mode)
	#[arg(short = 'm', long = "message", default_value = "ping")]
	message: String,
}

fn if_name_to_index(name: &str) -> Option<u32> {
	// Try Unix libc if available
	#[cfg(unix)]
	{
		use std::ffi::CString;
		let c = CString::new(name).ok()?;
		let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
		if idx == 0 {
			return None;
		} else {
			return Some(idx);
		}
	}

	// Try Windows API If_nametoindex via windows-sys if available
	#[cfg(windows)]
	{
		use std::ffi::CString;
		use windows_sys::Win32::NetworkManagement::IpHelper::If_nametoindex;
		let c = CString::new(name).ok()?;
		let idx = unsafe { If_nametoindex(c.as_ptr()) };
		if idx == 0 {
			return None;
		} else {
			return Some(idx);
		}
	}

	#[cfg(not(any(unix, windows)))]
	{
		None
	}
}

fn parse_iface(iface: &Option<String>) -> Result<u32> {
	if let Some(s) = iface {
		// try parse numeric first
		if let Ok(i) = s.parse::<u32>() {
			return Ok(i);
		}
		// try name -> index
		if let Some(idx) = if_name_to_index(s) {
			return Ok(idx);
		} else {
			bail!("could not resolve interface name '{}' to index; try passing numeric index instead", s);
		}
	}
	Ok(0) // 0 means "default" interface for many socket operations
}

fn make_recv_socket(port: u16, mcast: Ipv6Addr, iface_index: u32) -> Result<UdpSocket> {
	// Create IPv6 UDP socket using socket2 to set options then convert to std::net::UdpSocket
	let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
	// allow multiple listeners on same port/address on unix (SO_REUSEADDR). On windows this acts differently.
	sock.set_reuse_address(true)?;
	#[cfg(unix)]
	{
		// On unix it's usually useful to set reuse_port as well so multiple programs can bind same multicast port
		sock.set_reuse_port(true).ok();
	}
	// Bind to [::]:port (listen on all addresses)
	let addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
	sock.bind(&addr.into())?;

	// join multicast group
	sock.join_multicast_v6(&mcast, iface_index)?;

	// Convert to std UdpSocket
	let std_sock: UdpSocket = sock.into();
	std_sock.set_nonblocking(false)?;
	Ok(std_sock)
}

fn make_send_socket(iface_index: u32) -> Result<UdpSocket> {
	let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
	// allow reuse
	sock.set_reuse_address(true)?;
	#[cfg(unix)]
	{
		sock.set_reuse_port(true).ok();
	}
	// bind to ephemeral port on unspecified address (so recv_from works)
	let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
	sock.bind(&bind_addr.into())?;

	// set outgoing interface for multicast (0 means default)
	if iface_index != 0 {
		sock.set_multicast_if_v6(iface_index)?;
	}

	// optional: set hop limit for multicast so it can traverse multiple routers if desired
	// sock.set_multicast_hops_v6(10)?; // uncomment/change as needed

	let udp: UdpSocket = sock.into();
	Ok(udp)
}

fn run_server(args: &Args, iface_index: u32, mcast_addr: Ipv6Addr) -> Result<()> {
	println!("Starting server: join group {} port {} (iface_index={})", mcast_addr, args.port, iface_index);
	let sock = make_recv_socket(args.port, mcast_addr, iface_index)?;
	let message_reply = args.message.clone();

	// dead-simple loop: receive and reply to sender with unicast
	let mut buf = [0u8; 1500];
	loop {
		let (n, src) = match sock.recv_from(&mut buf) {
			Ok(s) => s,
			Err(e) => {
				eprintln!("recv error: {e}");
				continue;
			}
		};
		let data = &buf[..n];
		println!("received {} bytes from {}", n, src);
		// reply as unicast to src
		let reply_bytes = message_reply.as_bytes();
		match sock.send_to(reply_bytes, &src) {
			Ok(sent) => {
				println!("sent {} bytes reply to {}", sent, src);
			}
			Err(e) => {
				eprintln!("failed to send reply to {}: {}", src, e);
			}
		}
	}
}

fn run_client(args: &Args, iface_index: u32, mcast_addr: Ipv6Addr) -> Result<()> {
	println!(
		"Starting client: send multicast to {}:{} every {}ms (iface_index={})",
		mcast_addr, args.port, args.interval_ms, iface_index
	);

	let send_sock = make_send_socket(iface_index)?;
	let recv_sock = {
		// We'll bind a socket to the same ephemeral port to listen for replies.
		// This socket is separate and bound to ::, ephemeral port so remote servers can reply.
		let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
		s.set_reuse_address(true)?;
		#[cfg(unix)]
		{ s.set_reuse_port(true).ok(); }
		// bind to ephemeral port (0)
		s.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0).into())?;
		let u: UdpSocket = s.into();
		u
	};

	// We want to know what local port we send from so replies come back correctly.
	// If the send socket was bound to ephemeral, get its local addr.
	let local_addr = send_sock.local_addr().context("failed to get local addr")?;
	println!("local addr used for sending: {}", local_addr);

	// We'll use the same socket to send; make sure we set a read timeout on recv socket
	recv_sock
		.set_read_timeout(Some(Duration::from_millis(500)))
		.ok();

	// data structures for stats
	let stats = Arc::new(Mutex::new(ClientStats {
		total_sent: 0,
		total_replies: 0,
		per_server: HashMap::new(),
	}));

	// spawn receiver thread
	{
		let recv = recv_sock.try_clone().context("clone recv socket")?;
		let stats_rx = Arc::clone(&stats);
		thread::spawn(move || {
			let mut buf = [0u8; 1500];
			loop {
				match recv.recv_from(&mut buf) {
					Ok((n, src)) => {
						let data = &buf[..n];
						let key = src.ip();
						let mut st = stats_rx.lock().unwrap();
						st.total_replies += 1;
						let entry = st.per_server.entry(key).or_insert_with(|| ServerStat { replies: 0 });
						entry.replies += 1;
						println!("reply {} bytes from {}: {}", n, src, String::from_utf8_lossy(data));
					}
					Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
						// no data, continue
						thread::sleep(Duration::from_millis(10));
						continue;
					}
					Err(e) => {
						eprintln!("recv err: {}", e);
						thread::sleep(Duration::from_millis(100));
					}
				}
			}
		});
	}

	// send loop
	let target = SocketAddr::V6(SocketAddrV6::new(mcast_addr, args.port, 0, 0));
	let interval = Duration::from_millis(args.interval_ms.max(1));
	let stats_main = Arc::clone(&stats);
	let msg_bytes = args.message.clone().into_bytes();
	let mut next_print = Instant::now() + Duration::from_secs(5);

	loop {
		// send multicast packet
		match send_sock.send_to(&msg_bytes, &target) {
			Ok(sent) => {
				let mut st = stats_main.lock().unwrap();
				st.total_sent += 1;
				drop(st);
				// println!("sent {} bytes to {}", sent, target);
			}
			Err(e) => {
				eprintln!("send error: {}", e);
			}
		}

		// periodically print stats
		if Instant::now() >= next_print {
			let st = stats_main.lock().unwrap();
			let total_sent = st.total_sent.max(1); // avoid div by zero
			let total_replies = st.total_replies;
			let pct = (total_replies as f64) * 100.0 / (total_sent as f64);
			println!(
				"Total sent: {}  Total replies: {}  Reply %: {:.2}%",
				total_sent, total_replies, pct
			);
			if !st.per_server.is_empty() {
				println!("Per-server replies:");
				for (ip, s) in st.per_server.iter() {
					// estimate per-server reply % = replies / total_sent
					let p = (s.replies as f64) * 100.0 / (total_sent as f64);
					println!("  {} -> {} replies ({:.2}% of total requests)", ip, s.replies, p);
				}
			}
			next_print = Instant::now() + Duration::from_secs(5);
		}

		thread::sleep(interval);
	}
}

struct ServerStat {
	replies: u64,
}

struct ClientStats {
	total_sent: u64,
	total_replies: u64,
	per_server: HashMap<IpAddr, ServerStat>,
}

fn main() -> Result<()> {
	let args = Args::parse();

	// parse multicast IPv6 address
	let mcast_addr = Ipv6Addr::from_str(&args.group)
		.with_context(|| format!("invalid IPv6 address '{}'", args.group))?;

	// basic check it's a multicast address
	if !mcast_addr.is_multicast() {
		eprintln!("Warning: {} is not an IPv6 multicast address (continuing anyway)", mcast_addr);
	}

	let iface_index = parse_iface(&args.iface)?;

	if args.server {
		run_server(&args, iface_index, mcast_addr)?;
	} else {
		run_client(&args, iface_index, mcast_addr)?;
	}

	Ok(())
}
