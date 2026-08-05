//! Minimal network protocols on top of the e1000 driver. Currently
//! just ARP (Ethernet/IPv4): enough to resolve the gateway on QEMU's
//! user-mode network. The IPv4/UDP/TCP stack lands here next.

use alloc::vec::Vec;

pub const ETHERTYPE_ARP: u16 = 0x0806;
const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;

/// This guest's address on the QEMU user-mode network.
pub const OUR_IP: [u8; 4] = [10, 0, 2, 15];
/// QEMU user-net gateway (the host, NATed).
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// Sends an ARP request for `target_ip` and returns the responder's
/// MAC if a reply arrives within `timeout_ticks` (100 Hz ticks).
pub fn resolve(target_ip: [u8; 4], timeout_ticks: u64) -> Option<[u8; 6]> {
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
fn parse_arp_reply(frame: &[u8]) -> Option<[u8; 6]> {
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
