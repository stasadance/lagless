//! Just enough STUN parsing to demux a shared UDP port: read the local ICE ufrag
//! out of a Binding request's USERNAME (`<local ufrag>:<remote ufrag>`, RFC 8445 7.2.2).

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const ATTR_USERNAME: u16 = 0x0006;

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
    fn sdp() {
        assert_eq!(
            sdp_ufrag("v=0\r\na=ice-ufrag:AbCd\r\na=ice-pwd:x\r\n").as_deref(),
            Some("AbCd")
        );
    }
}
