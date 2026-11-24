//! Just enough STUN to demux a shared UDP port: read the local ICE ufrag out of a
//! Binding request's USERNAME (`<local ufrag>:<remote ufrag>`, RFC 8445 7.2.2).
//! Plus a bare Binding request/response (RFC 8489) to learn the server's public address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const ATTR_USERNAME: u16 = 0x0006;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Binding request with no attributes.
pub fn binding_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut m = [0u8; 20];
    m[..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    m[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    m[8..].copy_from_slice(txid);
    m
}

/// Transaction id and mapped address of a Binding success response, or None.
/// XOR-MAPPED-ADDRESS wins over MAPPED-ADDRESS (RFC 8489 14.1, 14.2).
pub fn binding_response(b: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    if b.len() < 20 || u16::from_be_bytes([b[0], b[1]]) != BINDING_SUCCESS {
        return None;
    }
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    if u32::from_be_bytes([b[4], b[5], b[6], b[7]]) != MAGIC || 20 + len > b.len() {
        return None;
    }
    let txid: [u8; 12] = b[8..20].try_into().ok()?;
    let (mut xor, mut plain) = (None, None);
    let mut i = 20;
    while i + 4 <= 20 + len {
        let at = u16::from_be_bytes([b[i], b[i + 1]]);
        let al = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        let v = b.get(i + 4..i + 4 + al)?;
        match at {
            ATTR_XOR_MAPPED_ADDRESS => xor = address(v, Some(&b[4..20])),
            ATTR_MAPPED_ADDRESS => plain = address(v, None),
            _ => {}
        }
        i += 4 + al.div_ceil(4) * 4;
    }
    Some((txid, xor.or(plain)?))
}

/// (XOR-)MAPPED-ADDRESS value; `key` is magic cookie + txid for the XOR form.
fn address(v: &[u8], key: Option<&[u8]>) -> Option<SocketAddr> {
    let x = |i: usize| key.map_or(0, |k| k[i]);
    if v.len() < 4 {
        return None;
    }
    let port = u16::from_be_bytes([v[2] ^ x(0), v[3] ^ x(1)]);
    let ip = match v[1] {
        1 if v.len() >= 8 => {
            let o: [u8; 4] = std::array::from_fn(|i| v[4 + i] ^ x(i));
            IpAddr::V4(Ipv4Addr::from(o))
        }
        2 if v.len() >= 20 => {
            let o: [u8; 16] = std::array::from_fn(|i| v[4 + i] ^ x(i));
            IpAddr::V6(Ipv6Addr::from(o))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Local ufrag of a STUN Binding request, or None for anything else.
pub fn binding_ufrag(b: &[u8]) -> Option<&str> {
    if b.len() < 20 || b[0] & 0xC0 != 0 {
        return None;
    }
    let ty = u16::from_be_bytes([b[0], b[1]]);
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    if ty != BINDING_REQUEST
        || u32::from_be_bytes([b[4], b[5], b[6], b[7]]) != MAGIC
        || 20 + len > b.len()
    {
        return None;
    }
    let mut i = 20;
    let end = 20 + len;
    while i + 4 <= end {
        let at = u16::from_be_bytes([b[i], b[i + 1]]);
        let al = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        let v = b.get(i + 4..i + 4 + al)?;
        if at == ATTR_USERNAME {
            let s = std::str::from_utf8(v).ok()?;
            return Some(s.split(':').next().unwrap_or(s));
        }
        i += 4 + al.div_ceil(4) * 4;
    }
    None
}

/// First `a=ice-ufrag:` in an SDP.
pub fn sdp_ufrag(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|l| l.trim().strip_prefix("a=ice-ufrag:"))
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(user: &[u8]) -> Vec<u8> {
        let mut attrs = vec![0x80, 0x22, 0, 3, b'a', b'b', b'c', 0]; // SOFTWARE, padded
        attrs.extend_from_slice(&ATTR_USERNAME.to_be_bytes());
        attrs.extend_from_slice(&(user.len() as u16).to_be_bytes());
        attrs.extend_from_slice(user);
        while attrs.len() % 4 != 0 {
            attrs.push(0);
        }
        let mut m = vec![0, 1];
        m.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        m.extend_from_slice(&MAGIC.to_be_bytes());
        m.extend_from_slice(&[7; 12]);
        m.extend(attrs);
        m
    }

    #[test]
    fn parses_username() {
        assert_eq!(binding_ufrag(&req(b"srvU:cliU")), Some("srvU"));
        assert_eq!(binding_ufrag(&req(b"x:y")), Some("x"));
    }

    #[test]
    fn rejects_non_stun() {
        assert_eq!(
            binding_ufrag(&[
                0x17, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]),
            None
        );
        assert_eq!(binding_ufrag(&req(b"a:b")[..22]), None);
        let mut resp = req(b"a:b");
        resp[1] = 0x01;
        resp[0] = 0x01; // binding success response
        assert_eq!(binding_ufrag(&resp), None);
    }

    #[test]
    fn mapped_address() {
        let txid = [9u8; 12];
        let req = binding_request(&txid);
        assert_eq!(binding_ufrag(&req), None);
        // XOR-MAPPED-ADDRESS 203.0.113.5:40000, RFC 8489 encoding
        let mut m = vec![0x01, 0x01, 0, 12];
        m.extend_from_slice(&MAGIC.to_be_bytes());
        m.extend_from_slice(&txid);
        m.extend_from_slice(&[0x00, 0x20, 0, 8, 0, 1]);
        m.extend_from_slice(&(40000u16 ^ 0x2112).to_be_bytes());
        for (i, o) in [203u8, 0, 113, 5].iter().enumerate() {
            m.push(o ^ MAGIC.to_be_bytes()[i]);
        }
        let (t, a) = binding_response(&m).unwrap();
        assert_eq!(t, txid);
        assert_eq!(a, "203.0.113.5:40000".parse::<SocketAddr>().unwrap());
        assert_eq!(binding_response(&req), None);
    }

    #[test]
    fn sdp() {
        assert_eq!(
            sdp_ufrag("v=0\r\na=ice-ufrag:AbCd\r\na=ice-pwd:x\r\n").as_deref(),
            Some("AbCd")
        );
    }
}
