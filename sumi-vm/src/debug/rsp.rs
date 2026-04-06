use std::io::{self, Read, Write};
use std::net::TcpStream;

/// Read one RSP packet from the stream. Returns the packet data (without framing).
/// Handles ACK/NACK and the leading '$', trailing '#xx'.
/// Also handles Ctrl+C (0x03) as an interrupt — returns None for interrupt.
pub fn read_packet(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut byte = [0u8; 1];

    // Skip any '+'/'-' ACKs and find '$' or 0x03
    loop {
        stream.read_exact(&mut byte)?;
        match byte[0] {
            b'$' => break,
            0x03 => return Ok(None), // Ctrl+C interrupt
            b'+' | b'-' => continue, // ACK/NACK, ignore
            _ => continue,
        }
    }

    // Read until '#'
    let mut data = Vec::with_capacity(256);
    loop {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'#' {
            break;
        }
        data.push(byte[0]);
    }

    // Read 2-char hex checksum
    let mut cksum_buf = [0u8; 2];
    stream.read_exact(&mut cksum_buf)?;

    // Verify checksum
    let expected = checksum(&data);
    let received = hex_byte(cksum_buf[0], cksum_buf[1]).unwrap_or(0);
    if expected != received {
        // Send NACK
        stream.write_all(b"-")?;
        stream.flush()?;
        // Retry by recursing (GDB will retransmit)
        return read_packet(stream);
    }

    // Send ACK
    stream.write_all(b"+")?;
    stream.flush()?;

    Ok(Some(data))
}

/// Send an RSP packet.
pub fn send_packet(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    let cksum = checksum(data);
    let mut pkt = Vec::with_capacity(data.len() + 4);
    pkt.push(b'$');
    pkt.extend_from_slice(data);
    pkt.push(b'#');
    pkt.push(hex_char(cksum >> 4));
    pkt.push(hex_char(cksum & 0xf));
    stream.write_all(&pkt)?;
    stream.flush()?;

    // Read ACK (but don't block forever — some GDB versions skip ACK after QStartNoAckMode)
    // For simplicity, we just read and ignore.
    let mut ack = [0u8; 1];
    let _ = stream.read(&mut ack);
    Ok(())
}

/// Send an empty response (unsupported command).
pub fn send_empty(stream: &mut TcpStream) -> io::Result<()> {
    send_packet(stream, b"")
}

/// Send "OK" response.
pub fn send_ok(stream: &mut TcpStream) -> io::Result<()> {
    send_packet(stream, b"OK")
}

/// Send an error response.
pub fn send_error(stream: &mut TcpStream, code: u8) -> io::Result<()> {
    let msg = format!("E{:02x}", code);
    send_packet(stream, msg.as_bytes())
}

/// Send a stop reply: "T05" (SIGTRAP) with optional reason.
pub fn send_stop_trap(stream: &mut TcpStream) -> io::Result<()> {
    // Signal 05 = SIGTRAP
    send_packet(stream, b"T05")
}

/// Send a stop reply with signal number.
pub fn send_stop_signal(stream: &mut TcpStream, signal: u8) -> io::Result<()> {
    let msg = format!("T{:02x}", signal);
    send_packet(stream, msg.as_bytes())
}

/// Encode bytes as hex string.
pub fn encode_hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2);
    for &b in data {
        out.push(hex_char(b >> 4));
        out.push(hex_char(b & 0xf));
    }
    out
}

/// Decode hex string to bytes.
pub fn decode_hex(hex: &[u8]) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.chunks(2) {
        out.push(hex_byte(chunk[0], chunk[1])?);
    }
    Some(out)
}

/// Parse a hex number from a byte slice.
pub fn parse_hex_num(data: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(data).ok()?;
    u64::from_str_radix(s, 16).ok()
}

fn checksum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

fn hex_char(nibble: u8) -> u8 {
    let n = nibble & 0xf;
    if n < 10 { b'0' + n } else { b'a' + n - 10 }
}

fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some((hex_val(hi)? << 4) | hex_val(lo)?)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
