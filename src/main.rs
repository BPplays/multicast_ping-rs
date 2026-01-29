// multicast_ping.rs
// A small async Rust program that can run in client or server mode.
// - Server mode (-s) binds to the given port, joins the specified IPv6 multicast group, and replies (unicast) to any incoming packet with an `ACK:` echo.
// - Client mode (default) periodically sends multicast "PING <seq>" packets every -n milliseconds and counts replies; prints running stats and a final summary on Ctrl+C.
//
// Usage examples:
//  Server: cargo run --release -- -s -a ff12c909:3199:e8ba:6f6f:7d23:e6ae:d85d -p 3000 -I eth0
//  Client: cargo run --release -- -a ff12c909:3199:e8ba:6f6f:7d23:e6ae:d85d -p 3000 -n 500 -t 800
// Note: some systems require an explicit interface when joining IPv6 multicast. Use -I/--ifname to pass the interface name (e.g. eth0). The program converts the name to an index using libc::if_nametoindex.
// Add `libc = "0.2"` to your Cargo.toml dependencies if not already present.

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;
use std::ffi::CString;

use clap::Parser;
use tokio::sync::atomic::{AtomicUsize, Ordering};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
	/// Run in server mode (listen on multicast and reply to senders)
	#[arg(short = 's', long)]
	server: bool,

	/// Multicast IPv6 address to use
	#[arg(short = 'a', long, default_value = "ff12c909:3199:e8ba:6f6f:7d23:e6ae:d85d")]
	maddr: String,

	/// Port to use for multicast/unicast replies
	#[arg(short = 'p', long, default_value_t = 3000)]
	port: u16,

	/// Interval between multicast requests (ms) (client mode)
	#[arg(short = 'n', long, default_value_t = 1000)]
	interval_ms: u64,

	/// Timeout waiting for replies per probe (ms) (client mode)
	#[arg(short = 't', long, default_value_t = 500)]
	timeout_ms: u64,

	/// Optional interface name for joining multicast (server; e.g. eth0). If omitted uses index 0 (system default).
	#[arg(short = 'I', long = "ifname")]
	if_name: Option<String>,
}

fn try_fix_ipv6_str(s: &str) -> String {
	// Attempt to fix "weird" segments longer than 4 hex digits by splitting into 4-char chunks.
	// e.g. `ff12c909:3199:...` -> `ff12:c909:3199:...`
	let parts: Vec<String> = s
		.split(':')
		.flat_map(|seg| {
			if seg.len() <= 4 {
				vec![seg.to_string()]
			} else {
				// split into 4-char chunks
				let bytes = seg.as_bytes();
				let mut out = Vec::new();
				let mut i = 0;
				while i < bytes.len() {
					let end = std::cmp::min(i + 4, bytes.len());
					out.push(String::from_utf8(bytes[i..end].to_vec()).unwrap());
					i += 4;
				}
				out
			}
		})
		.collect();
	parts.join(":")
}

fn parse_ipv6_addr(s: &str) -> anyhow::Result<Ipv6Addr> {
	// Try direct parse first
	if let Ok(v) = s.parse::<Ipv6Addr>() {
		return Ok(v);
	}
	// Try fixing long segments
	let fixed = try_fix_ipv6_str(s);
	if let Ok(v) = fixed.parse::<Ipv6Addr>() {
		println!("Note: fixed multicast address from '{}' -> '{}'", s, fixed);
		return Ok(v);
	}
	anyhow::bail!("failed to parse IPv6 address '{}', tried '{}'", s, fixed)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	env_logger::init();
	let args = Args::parse();

	let multicast = parse_ipv6_addr(&args.maddr)?;
	let multicast_sock = SocketAddr::V6(SocketAddrV6::new(multicast, args.port, 0, 0));

	if args.server {
		run_server(multicast, args.port, args.if_name.clone()).await?;
	} else {
		run_client(multicast_sock, args.interval_ms, args.timeout_ms).await?;
	}

	Ok(())
}

async fn run_server(multicast: Ipv6Addr, port: u16, if_name: Option<String>) -> anyhow::Result<()> {
	// Convert optional interface name to index using libc::if_nametoindex
	let if_index: u32 = if let Some(name) = if_name {
		let c = CString::new(name.clone()).map_err(|e| anyhow::anyhow!("invalid interface name: {}", e))?;
		// SAFETY: if_nametoindex is a C function; it returns 0 on failure
		let idx = unsafe { libc::if_nametoindex(c.as_ptr()) } as u32;
		if idx == 0 {
			anyhow::bail!("interface name '{}' not found or cannot be converted to index", name);
		}
		idx
	} else {
		0u32
	};

	println!("Starting server: joining multicast {} on port {} (if_index={})", multicast, port, if_index);

	// Bind to wildcard IPv6 on the port
	let bind_addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0));
	let std_sock = std::net::UdpSocket::bind(bind_addr)?;
	std_sock.set_nonblocking(true)?;

	// Join multicast group
	std_sock.join_multicast_v6(&multicast, if_index)?;

	// Enable multicast loopback so replies can be observed locally if needed
	let _ = std_sock.set_multicast_loop_v6(true);

	let sock = tokio::net::UdpSocket::from_std(std_sock)?;

	let mut buf = vec![0u8; 1500];
	loop {
		let (len, src) = sock.recv_from(&mut buf).await?;
		let payload = &buf[..len];
		log::info!("received {} bytes from {}", len, src);

		// Spawn a small task to reply so we don't block receiving loop
		let sock_clone = sock.clone();
		let resp = payload.to_vec();
		tokio::spawn(async move {
			// Send back a short acknowledgement; echo payload prefixed with "ACK:"
			let mut out = Vec::new();
			out.extend_from_slice(b"ACK:");
			out.extend_from_slice(&resp);
			if let Err(e) = sock_clone.send_to(&out, src).await {
				log::warn!("failed to send reply to {}: {}", src, e);
			}
		});
	}
}

async fn run_client(multicast_sock: SocketAddr, interval_ms: u64, timeout_ms: u64) -> anyhow::Result<()> {
	println!("Starting client: sending to {} every {} ms, timeout {} ms", multicast_sock, interval_ms, timeout_ms);

	// Bind a local socket to receive unicast replies. Use unspecified IPv6 address with ephemeral port.
	let bind_addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));
	let std_sock = std::net::UdpSocket::bind(bind_addr)?;
	std_sock.set_nonblocking(true)?;

	let sock = tokio::net::UdpSocket::from_std(std_sock)?;

	let sent = Arc::new(AtomicUsize::new(0));
	let recv = Arc::new(AtomicUsize::new(0));

	// Receiver task
	let recv_sock = sock.clone();
	let recv_counter = recv.clone();
	tokio::spawn(async move {
		let mut buf = vec![0u8; 1500];
		loop {
			match recv_sock.recv_from(&mut buf).await {
				Ok((len, src)) => {
					log::info!("client got {} bytes from {}", len, src);
					recv_counter.fetch_add(1, Ordering::Relaxed);
				}
				Err(e) => {
					// non-fatal: continue
					log::debug!("recv error: {}", e);
					tokio::time::sleep(Duration::from_millis(10)).await;
				}
			}
		}
	});

	// Sender loop
	let sender = sock.clone();
	let sent_counter = sent.clone();
	let mut seq: u64 = 0;
	let interval = Duration::from_millis(interval_ms);
	let timeout = Duration::from_millis(timeout_ms);

	let stats_printer = tokio::spawn({
		let sent = sent.clone();
		let recv = recv.clone();
		async move {
			loop {
				tokio::time::sleep(Duration::from_secs(5)).await;
				let s = sent.load(Ordering::Relaxed);
				let r = recv.load(Ordering::Relaxed);
				let pct = if s == 0 { 0.0 } else { (r as f64) * 100.0 / (s as f64) };
				println!("sent={} recv={} success={:.2}%", s, r, pct);
			}
		}
	});

	let send_task = tokio::spawn(async move {
		loop {
			seq += 1;
			let msg = format!("PING {}", seq);
			if let Err(e) = sender.send_to(msg.as_bytes(), multicast_sock).await {
				log::warn!("failed to send ping {}: {}", seq, e);
			} else {
				sent_counter.fetch_add(1, Ordering::Relaxed);
			}
			tokio::time::sleep(interval).await;
		}
	});

	// Wait for ctrl+c and then print final stats
	tokio::select! {
		_ = tokio::signal::ctrl_c() => {
			println!("Ctrl-C received, printing stats...");
		}
		_ = send_task => {}
	}

	let s = sent.load(Ordering::Relaxed);
	let r = recv.load(Ordering::Relaxed);
	let pct = if s == 0 { 0.0 } else { (r as f64) * 100.0 / (s as f64) };
	println!("FINAL: sent={} recv={} success={:.2}%", s, r, pct);

	// ensure stats_printer finishes (it won't by itself) — just abort
	stats_printer.abort();

	Ok(())
}
