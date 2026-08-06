//! Minimal network stack over the e1000 driver: Ethernet framing,
//! ARP (with a small cache), IPv4, UDP, and a minimal TCP client —
//! enough for request/response traffic through QEMU's user-mode
//! network (guest 10.0.2.15, gateway 10.0.2.2, DNS 10.0.2.3).

use alloc::vec;
use alloc::vec::Vec;

pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;
pub(crate) const IP_PROTO_UDP: u8 = 17;
pub(crate) const IP_PROTO_TCP: u8 = 6;

/// This guest's address on the QEMU user-mode network.
pub const OUR_IP: [u8; 4] = [10, 0, 2, 15];
/// QEMU user-net gateway (the host, NATed).
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// How long to wait for an ARP reply (100 Hz ticks).
const ARP_TIMEOUT_TICKS: u64 = 300;

static ARP_CACHE: crate::sync::SpinLock<Vec<([u8; 4], [u8; 6])>> =
    crate::sync::SpinLock::new(Vec::new());

// ---------------------------------------------------------------------
// ARP
// ---------------------------------------------------------------------

/// Resolves `target_ip` to a MAC address, using the cache first and
/// falling back to an ARP request. NOTE: while waiting for the reply,
/// any other received frames are dropped.
pub fn resolve(target_ip: [u8; 4], timeout_ticks: u64) -> Option<[u8; 6]> {
    {
        let cache = ARP_CACHE.lock();
        if let Some((_, mac)) = cache.iter().find(|(ip, _)| *ip == target_ip) {
            return Some(*mac);
        }
    }
    let mac = crate::e1000::mac()?;
    let mut pkt = Vec::with_capacity(42);
    pkt.extend_from_slice(&[0xFF; 6]); // destination: broadcast
    pkt.extend_from_slice(&mac); // source
    pkt.extend_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    pkt.extend_from_slice(&[0, 1]); // hardware type: Ethernet
    pkt.extend_from_slice(&[0x08, 0x00]); // protocol: IPv4
    pkt.extend_from_slice(&[6, 4]); // hlen, plen
    pkt.extend_from_slice(&ARP_REQUEST.to_be_bytes());
    pkt.extend_from_slice(&mac); // sender hardware address
    pkt.extend_from_slice(&OUR_IP); // sender protocol address
    pkt.extend_from_slice(&[0; 6]); // target hardware: unknown
    pkt.extend_from_slice(&target_ip); // target protocol address
    crate::e1000::send(&pkt).ok()?;

    let deadline = crate::interrupts::ticks() + timeout_ticks;
    loop {
        if let Some(frame) = crate::e1000::poll_rx() {
            if let Some(sha) = parse_arp_reply(&frame) {
                let mut cache = ARP_CACHE.lock();
                cache.retain(|(ip, _)| *ip != target_ip);
                cache.push((target_ip, sha));
                if cache.len() > 8 {
                    cache.remove(0);
                }
                return Some(sha);
            }
        }
        if crate::interrupts::ticks() >= deadline {
            return None;
        }
        // Yield to other tasks while waiting for the reply.
        crate::interrupts::halt();
    }
}

/// Extracts the sender hardware address from an ARP reply frame.
pub(crate) fn parse_arp_reply(frame: &[u8]) -> Option<[u8; 6]> {
    if frame.len() < 42 {
        return None;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETHERTYPE_ARP {
        return None;
    }
    let op = u16::from_be_bytes([frame[20], frame[21]]);
    if op != ARP_REPLY {
        return None;
    }
    Some([frame[22], frame[23], frame[24], frame[25], frame[26], frame[27]])
}

/// The sender's IPv4 address carried by an ARP reply frame (bytes
/// 28..32 of the frame — right after the sender hardware address).
pub(crate) fn arp_reply_ip(frame: &[u8]) -> Option<[u8; 4]> {
    if frame.len() < 42 {
        return None;
    }
    Some([frame[28], frame[29], frame[30], frame[31]])
}

/// Checks the ARP cache (no wire traffic).
pub(crate) fn arp_lookup(target_ip: [u8; 4]) -> Option<[u8; 6]> {
    let cache = ARP_CACHE.lock();
    cache.iter().find(|(ip, _)| *ip == target_ip).map(|(_, mac)| *mac)
}

/// Inserts/updates a cache entry.
pub(crate) fn cache_arp(ip: [u8; 4], mac: [u8; 6]) {
    let mut cache = ARP_CACHE.lock();
    cache.retain(|(i, _)| *i != ip);
    cache.push((ip, mac));
    if cache.len() > 8 {
        cache.remove(0);
    }
}

/// Sends an ARP request for `target_ip` without waiting for the reply —
/// the non-blocking counterpart to `resolve` (a syscall handler can't
/// `hlt`-wait for the reply; callers poll `arp_lookup`/the RX ring).
pub(crate) fn send_arp_request(target_ip: [u8; 4]) -> Result<(), &'static str> {
    let mac = crate::e1000::mac().ok_or("NIC down")?;
    let mut pkt = Vec::with_capacity(42);
    pkt.extend_from_slice(&[0xFF; 6]); // destination: broadcast
    pkt.extend_from_slice(&mac); // source
    pkt.extend_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    pkt.extend_from_slice(&[0, 1]); // hardware type: Ethernet
    pkt.extend_from_slice(&[0x08, 0x00]); // protocol: IPv4
    pkt.extend_from_slice(&[6, 4]); // hlen, plen
    pkt.extend_from_slice(&ARP_REQUEST.to_be_bytes());
    pkt.extend_from_slice(&mac); // sender hardware address
    pkt.extend_from_slice(&OUR_IP); // sender protocol address
    pkt.extend_from_slice(&[0; 6]); // target hardware: unknown
    pkt.extend_from_slice(&target_ip); // target protocol address
    crate::e1000::send(&pkt)
}

// ---------------------------------------------------------------------
// IPv4
// ---------------------------------------------------------------------

/// Builds and sends an IPv4 packet to `dst_ip` (resolving its MAC via
/// ARP first). Kernel-internal only — the `hlt`-based resolver can't
/// run inside a syscall handler (see `send_ipv4_mac`).
pub fn send_ipv4(dst_ip: [u8; 4], proto: u8, payload: &[u8]) -> Result<(), &'static str> {
    let dst_mac = resolve(dst_ip, ARP_TIMEOUT_TICKS).ok_or("ARP failed")?;
    send_ipv4_mac(dst_mac, dst_ip, proto, payload)
}

/// Builds and sends an IPv4 packet to `dst_ip` via an already-known
/// `dst_mac` — no ARP, no waiting, safe to call from a syscall handler.
pub(crate) fn send_ipv4_mac(
    dst_mac: [u8; 6],
    dst_ip: [u8; 4],
    proto: u8,
    payload: &[u8],
) -> Result<(), &'static str> {
    let src_mac = crate::e1000::mac().ok_or("NIC down")?;
    let mut frame = vec![0u8; 14 + 20 + payload.len()];
    frame[0..6].copy_from_slice(&dst_mac);
    frame[6..12].copy_from_slice(&src_mac);
    frame[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let ip = &mut frame[14..34];
    ip[0] = 0x45; // IPv4, 5 words
    ip[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&0u16.to_be_bytes()); // id
    ip[6..8].copy_from_slice(&0u16.to_be_bytes()); // flags/frag
    ip[8] = 64; // ttl
    ip[9] = proto;
    ip[10..12].copy_from_slice(&0u16.to_be_bytes()); // checksum (computed below)
    ip[12..16].copy_from_slice(&OUR_IP);
    ip[16..20].copy_from_slice(&dst_ip);
    let csum = checksum(ip);
    ip[10..12].copy_from_slice(&csum.to_be_bytes());
    frame[34..].copy_from_slice(payload);
    crate::e1000::send(&frame)
}

/// Parses an Ethernet frame into an IPv4 packet: (src_ip, dst_ip,
/// protocol, payload slice). Returns `None` for non-IPv4 or malformed
/// frames.
pub(crate) fn parse_ipv4(frame: &[u8]) -> Option<([u8; 4], [u8; 4], u8, &[u8])> {
    if frame.len() < 14 + 20 {
        return None;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != ETHERTYPE_IPV4 {
        return None;
    }
    let ihl = (frame[14] & 0x0F) as usize * 4;
    if ihl < 20 || frame.len() < 14 + ihl {
        return None;
    }
    let total = u16::from_be_bytes([frame[16], frame[17]]) as usize;
    let end = (14 + total).min(frame.len());
    let src = [frame[26], frame[27], frame[28], frame[29]];
    let dst = [frame[30], frame[31], frame[32], frame[33]];
    let proto = frame[23];
    Some((src, dst, proto, &frame[14 + ihl..end]))
}

// ---------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------

/// Builds and sends a UDP datagram.
pub fn send_udp(
    dst_ip: [u8; 4],
    dst_port: u16,
    src_port: u16,
    payload: &[u8],
) -> Result<(), &'static str> {
    let datagram = build_udp(dst_ip, dst_port, src_port, payload);
    send_ipv4(dst_ip, IP_PROTO_UDP, &datagram)
}

/// Sends a UDP datagram via an already-known MAC — the syscall-safe
/// variant (no ARP resolution, which needs a halt-loop the syscall
/// context can't do).
pub(crate) fn send_udp_mac(
    dst_mac: [u8; 6],
    dst_ip: [u8; 4],
    dst_port: u16,
    src_port: u16,
    payload: &[u8],
) -> Result<(), &'static str> {
    let datagram = build_udp(dst_ip, dst_port, src_port, payload);
    send_ipv4_mac(dst_mac, dst_ip, IP_PROTO_UDP, &datagram)
}

fn build_udp(dst_ip: [u8; 4], dst_port: u16, src_port: u16, payload: &[u8]) -> Vec<u8> {
    let len = 8 + payload.len();
    let mut udp = vec![0u8; len];
    udp[0..2].copy_from_slice(&src_port.to_be_bytes());
    udp[2..4].copy_from_slice(&dst_port.to_be_bytes());
    udp[4..6].copy_from_slice(&(len as u16).to_be_bytes());
    udp[8..].copy_from_slice(payload);
    // Checksum over the pseudo-header + datagram.
    let mut pseudo = Vec::with_capacity(12 + len);
    pseudo.extend_from_slice(&OUR_IP);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(IP_PROTO_UDP);
    pseudo.extend_from_slice(&(len as u16).to_be_bytes());
    pseudo.extend_from_slice(&udp);
    let csum = checksum(&pseudo);
    udp[6..8].copy_from_slice(&csum.to_be_bytes());
    udp
}

/// A received UDP datagram.
pub struct UdpPacket {
    pub src_ip: [u8; 4],
    pub src_port: u16,
    pub payload: Vec<u8>,
}

/// Drains the RX ring, returning the next UDP datagram addressed to
/// `dst_port` (other frames are dropped).
pub fn poll_udp(dst_port: u16) -> Option<UdpPacket> {
    while let Some(frame) = crate::e1000::poll_rx() {
        let Some((src_ip, _, proto, payload)) = parse_ipv4(&frame) else {
            continue;
        };
        if proto != IP_PROTO_UDP || payload.len() < 8 {
            continue;
        }
        let sport = u16::from_be_bytes([payload[0], payload[1]]);
        let dport = u16::from_be_bytes([payload[2], payload[3]]);
        if dport != dst_port {
            continue;
        }
        return Some(UdpPacket {
            src_ip,
            src_port: sport,
            payload: payload[8..].to_vec(),
        });
    }
    None
}

/// Sends `payload` to the gateway's `port` and waits for the echo,
/// returning how many reply bytes were copied into `reply`.
pub fn udp_echo(
    port: u16,
    payload: &[u8],
    reply: &mut [u8],
    timeout_ticks: u64,
) -> Result<usize, &'static str> {
    const SRC_PORT: u16 = 5555;
    send_udp(GATEWAY_IP, port, SRC_PORT, payload)?;
    let deadline = crate::interrupts::ticks() + timeout_ticks;
    loop {
        if let Some(pkt) = poll_udp(SRC_PORT) {
            if pkt.src_port == port {
                let n = pkt.payload.len().min(reply.len());
                reply[..n].copy_from_slice(&pkt.payload[..n]);
                return Ok(n);
            }
        }
        if crate::interrupts::ticks() >= deadline {
            return Err("UDP echo timeout");
        }
        crate::interrupts::halt();
    }
}

// ---------------------------------------------------------------------
// TCP (minimal client)
// ---------------------------------------------------------------------

const TCP_SYN: u8 = 0x02;
const TCP_ACK: u8 = 0x10;
const TCP_FIN: u8 = 0x01;
const TCP_PSH: u8 = 0x08;

/// A received TCP segment.
pub(crate) struct TcpSegment {
    pub(crate) src_port: u16,
    pub(crate) dst_port: u16,
    pub(crate) seq: u32,
    pub(crate) ack: u32,
    pub(crate) flags: u8,
    pub(crate) payload: Vec<u8>,
}

/// Parses a TCP segment out of an IPv4 payload.
pub(crate) fn parse_tcp(payload: &[u8]) -> Option<TcpSegment> {
    if payload.len() < 20 {
        return None;
    }
    let data_offset = ((payload[12] >> 4) as usize) * 4;
    if data_offset < 20 || payload.len() < data_offset {
        return None;
    }
    Some(TcpSegment {
        src_port: u16::from_be_bytes([payload[0], payload[1]]),
        dst_port: u16::from_be_bytes([payload[2], payload[3]]),
        seq: u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
        ack: u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]),
        flags: payload[13],
        payload: payload[data_offset..].to_vec(),
    })
}

/// One-shot TCP client exchange against `ip:port`: open the connection,
/// send `request`, read the response into `reply`, and close. Returns
/// the number of reply bytes received. No retransmission, no window
/// scaling — fine for a local slirp link.
///
/// When `fin_when_echoed` is true, the client sends its FIN as soon as
/// it has received `request.len()` bytes (echo-server semantics — the
/// server only closes after seeing EOF). When false, it waits for the
/// server to close the connection (HTTP/1.0 semantics).
pub fn tcp_request(
    ip: [u8; 4],
    port: u16,
    request: &[u8],
    reply: &mut [u8],
    timeout_ticks: u64,
    fin_when_echoed: bool,
) -> Result<usize, &'static str> {
    const SRC_PORT: u16 = 7777;
    let dst_mac = resolve(ip, ARP_TIMEOUT_TICKS).ok_or("ARP failed")?;
    let mut our_seq: u32 = 0x1234_5678;
    let mut their_seq: Option<u32> = None;
    let mut got = 0usize;
    let mut sent_data = false;
    let mut we_finned = false;

    // SYN (seq = our_seq).
    let mut seg = build_tcp(SRC_PORT, port, our_seq, 0, TCP_SYN, &[], ip);
    send_tcp(&dst_mac, ip, &seg)?;
    our_seq = our_seq.wrapping_add(1);

    let deadline = crate::interrupts::ticks() + timeout_ticks;
    loop {
        // Send the request data as soon as the handshake completes —
        // the server is waiting for it, so this must not depend on the
        // next received segment.
        if let Some(their) = their_seq {
            if !sent_data {
                sent_data = true;
                seg = build_tcp(
                    SRC_PORT,
                    port,
                    our_seq,
                    their.wrapping_add(1),
                    TCP_PSH | TCP_ACK,
                    request,
                    ip,
                );
                send_tcp(&dst_mac, ip, &seg)?;
                our_seq = our_seq.wrapping_add(request.len() as u32);
            }
        }
        if let Some(frame) = crate::e1000::poll_rx() {
            let Some((src_ip, _, proto, payload)) = parse_ipv4(&frame) else {
                continue;
            };
            if proto != IP_PROTO_TCP {
                continue;
            }
            let Some(s) = parse_tcp(payload) else { continue };
            if s.src_port != port {
                continue;
            }
            if their_seq.is_none() {
                // Expect the SYN-ACK.
                if s.flags & (TCP_SYN | TCP_ACK) == TCP_SYN | TCP_ACK {
                    their_seq = Some(s.seq);
                    our_seq = s.ack; // server acknowledged our SYN
                    // ACK their SYN.
                    seg = build_tcp(
                        SRC_PORT,
                        port,
                        our_seq,
                        s.seq.wrapping_add(1),
                        TCP_ACK,
                        &[],
                        ip,
                    );
                    send_tcp(&dst_mac, ip, &seg)?;
                }
                continue;
            }
            let their = their_seq.unwrap();
            // Data from the server: seq starts at their+1.
            if !s.payload.is_empty() {
                let base = their.wrapping_add(1);
                let offset = s.seq.wrapping_sub(base) as usize;
                if offset < reply.len() {
                    let n = (s.payload.len()).min(reply.len() - offset);
                    reply[offset..offset + n].copy_from_slice(&s.payload[..n]);
                    got = got.max(offset + n);
                    // ACK what we received.
                    seg = build_tcp(
                        SRC_PORT,
                        port,
                        our_seq,
                        s.seq.wrapping_add(s.payload.len() as u32),
                        TCP_ACK,
                        &[],
                        ip,
                    );
                    send_tcp(&dst_mac, ip, &seg)?;
                    // Response complete (echo semantics: the reply is as
                    // long as the request): close our side; the server
                    // sees EOF, closes, and FINs back.
                    if fin_when_echoed && got >= request.len() && !we_finned {
                        we_finned = true;
                        seg = build_tcp(
                            SRC_PORT,
                            port,
                            our_seq,
                            s.seq.wrapping_add(s.payload.len() as u32),
                            TCP_ACK | TCP_FIN,
                            &[],
                            ip,
                        );
                        send_tcp(&dst_mac, ip, &seg)?;
                    }
                }
            }
            if s.flags & TCP_FIN != 0 {
                // The server closed: the exchange is complete.
                return Ok(got);
            }
        }
        if crate::interrupts::ticks() >= deadline {
            return Err("TCP exchange timeout");
        }
        crate::interrupts::halt();
    }
}

/// Builds a TCP segment (header + payload) with a valid checksum.
pub(crate) fn build_tcp(
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
    dst_ip: [u8; 4],
) -> Vec<u8> {
    let mut seg = vec![0u8; 20 + payload.len()];
    seg[0..2].copy_from_slice(&src_port.to_be_bytes());
    seg[2..4].copy_from_slice(&dst_port.to_be_bytes());
    seg[4..8].copy_from_slice(&seq.to_be_bytes());
    seg[8..12].copy_from_slice(&ack.to_be_bytes());
    seg[12] = 5 << 4; // data offset
    seg[13] = flags;
    seg[14..16].copy_from_slice(&8192u16.to_be_bytes()); // window
    seg[16..18].copy_from_slice(&0u16.to_be_bytes()); // checksum
    seg[18..20].copy_from_slice(&0u16.to_be_bytes()); // urgent
    seg[20..].copy_from_slice(payload);
    // Checksum over the pseudo-header + segment.
    let len = seg.len();
    let mut pseudo = Vec::with_capacity(12 + len);
    pseudo.extend_from_slice(&OUR_IP);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(IP_PROTO_TCP);
    pseudo.extend_from_slice(&(len as u16).to_be_bytes());
    pseudo.extend_from_slice(&seg);
    let csum = checksum(&pseudo);
    seg[16..18].copy_from_slice(&csum.to_be_bytes());
    seg
}

/// Sends a raw TCP segment (Ethernet + IPv4 + TCP).
pub(crate) fn send_tcp(dst_mac: &[u8; 6], dst_ip: [u8; 4], seg: &[u8]) -> Result<(), &'static str> {
    let src_mac = crate::e1000::mac().ok_or("NIC down")?;
    let mut frame = vec![0u8; 14 + 20 + seg.len()];
    frame[0..6].copy_from_slice(dst_mac);
    frame[6..12].copy_from_slice(&src_mac);
    frame[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let ip = &mut frame[14..34];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((20 + seg.len()) as u16).to_be_bytes());
    ip[8] = 64;
    ip[9] = IP_PROTO_TCP;
    ip[12..16].copy_from_slice(&OUR_IP);
    ip[16..20].copy_from_slice(&dst_ip);
    let csum = checksum(ip);
    ip[10..12].copy_from_slice(&csum.to_be_bytes());
    frame[34..].copy_from_slice(seg);
    crate::e1000::send(&frame)
}

// ---------------------------------------------------------------------
// DNS
// ---------------------------------------------------------------------

/// QEMU user-net's built-in DNS forwarder.
pub const DNS_IP: [u8; 4] = [10, 0, 2, 3];

/// Resolves `name` to an IPv4 address via the user-net DNS forwarder.
pub fn dns_lookup(name: &str, timeout_ticks: u64) -> Option<[u8; 4]> {
    const SRC_PORT: u16 = 5353;
    let mut query = Vec::with_capacity(64);
    query.extend_from_slice(&0xBEEFu16.to_be_bytes()); // transaction id
    query.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: RD
    query.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    query.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    query.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    query.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    for label in name.split('.') {
        if label.len() > 63 {
            return None;
        }
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&1u16.to_be_bytes()); // QTYPE: A
    query.extend_from_slice(&1u16.to_be_bytes()); // QCLASS: IN
    send_udp(DNS_IP, 53, SRC_PORT, &query).ok()?;

    let deadline = crate::interrupts::ticks() + timeout_ticks;
    loop {
        if let Some(pkt) = poll_udp(SRC_PORT) {
            if pkt.src_port == 53 {
                return parse_dns_a(&pkt.payload);
            }
        }
        if crate::interrupts::ticks() >= deadline {
            return None;
        }
        crate::interrupts::halt();
    }
}

/// Extracts the first A record from a DNS reply.
fn parse_dns_a(reply: &[u8]) -> Option<[u8; 4]> {
    if reply.len() < 12 {
        return None;
    }
    let ancount = u16::from_be_bytes([reply[6], reply[7]]);
    if ancount == 0 {
        return None;
    }
    let mut pos = 12usize;
    // Skip the question section (name + qtype + qclass).
    loop {
        let len = *reply.get(pos)?;
        pos += 1;
        if len == 0 {
            break;
        }
        if len & 0xC0 == 0xC0 {
            pos += 1; // compression pointer
            break;
        }
        pos += len as usize;
    }
    pos += 4;
    // Walk answers looking for an A record.
    for _ in 0..ancount {
        loop {
            let len = *reply.get(pos)?;
            pos += 1;
            if len == 0 {
                break;
            }
            if len & 0xC0 == 0xC0 {
                pos += 1;
                break;
            }
            pos += len as usize;
        }
        if pos + 10 > reply.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([reply[pos], reply[pos + 1]]);
        pos += 2; // type
        pos += 2; // class
        pos += 4; // ttl
        let rdlen = u16::from_be_bytes([reply[pos], reply[pos + 1]]) as usize;
        pos += 2;
        if rtype == 1 && rdlen == 4 && pos + 4 <= reply.len() {
            return Some([reply[pos], reply[pos + 1], reply[pos + 2], reply[pos + 3]]);
        }
        pos += rdlen;
    }
    None
}

// ---------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------

/// Sends an HTTP/1.0 GET for `path` to `ip:port` and returns the raw
/// response (headers + body) in `reply`.
pub fn http_get(
    ip: [u8; 4],
    port: u16,
    path: &str,
    reply: &mut [u8],
    timeout_ticks: u64,
) -> Result<usize, &'static str> {
    let mut request = Vec::with_capacity(128);
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(path.as_bytes());
    request.extend_from_slice(b" HTTP/1.0\r\nHost: ");
    let mut ipbuf = [0u8; 16];
    request.extend_from_slice(crate::io::sprint(
        &mut ipbuf,
        format_args!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]),
    ).as_bytes());
    request.extend_from_slice(b"\r\n\r\n");
    // HTTP/1.0: the server closes the connection after the response.
    tcp_request(ip, port, &request, reply, timeout_ticks, false)
}

/// Splits an HTTP response into its body (everything after the header
/// terminator).
pub fn http_body(response: &[u8]) -> &[u8] {
    response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| &response[i + 4..])
        .unwrap_or(response)
}

/// Parses a dotted-quad string into an IPv4 address.
pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut parts = s.split('.');
    let mut out = [0u8; 4];
    for byte in out.iter_mut() {
        *byte = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

/// One's-complement Internet checksum.
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Formats a MAC for printing without allocating.
pub fn mac_to_str<'a>(mac: &[u8; 6], buf: &'a mut [u8; 24]) -> &'a str {
    crate::io::sprint(
        buf,
        format_args!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        ),
    )
}
