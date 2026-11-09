//! Invite tickets: `"mesh1"` + lower-case base32 of `1 || postcard(ticket) || crc32_be`.

use crate::ids::{MeshId, NodeAddrBytes, Role};
use crate::{FORMAT_VERSION, ProtoError};
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "mesh1";

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct InviteTicket {
    pub mesh: MeshId,
    pub inviter: NodeAddrBytes,
    pub token: [u8; 32],
    pub role: Role,
    pub expires_ms: u64,
}

impl InviteTicket {
    pub fn to_text(&self) -> String {
        let mut bytes = vec![FORMAT_VERSION];
        // Serializing this plain struct into a Vec cannot fail.
        if let Ok(b) = postcard::to_allocvec(self) {
            bytes.extend_from_slice(&b);
        }
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        format!("{PREFIX}{}", BASE32_NOPAD.encode(&bytes).to_lowercase())
    }

    pub fn from_text(s: &str) -> Result<Self, ProtoError> {
        let rest = s
            .trim()
            .strip_prefix(PREFIX)
            .ok_or(ProtoError::Ticket("missing prefix"))?;
        let bytes = BASE32_NOPAD
            .decode(rest.to_uppercase().as_bytes())
            .map_err(|_| ProtoError::Ticket("bad base32"))?;
        if bytes.len() < 5 {
            return Err(ProtoError::Ticket("too short"));
        }
        let (data, crc) = bytes.split_at(bytes.len() - 4);
        if crc != crc32fast::hash(data).to_be_bytes() {
            return Err(ProtoError::Ticket("checksum mismatch"));
        }
        if data[0] != FORMAT_VERSION {
            return Err(ProtoError::Ticket("unsupported version"));
        }
        let (t, trailing) = postcard::take_from_bytes::<InviteTicket>(&data[1..])
            .map_err(|_| ProtoError::Ticket("bad payload"))?;
        if !trailing.is_empty() {
            return Err(ProtoError::Ticket("trailing bytes"));
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> InviteTicket {
        InviteTicket {
            mesh: MeshId([7; 32]),
            inviter: NodeAddrBytes(vec![1, 2, 3, 4]),
            token: [5; 32],
            role: Role::Client,
            expires_ms: 123_456,
        }
    }

    fn flip_last_char(s: &mut String) {
        let last = s.pop().unwrap();
        s.push(if last == 'a' { 'b' } else { 'a' });
    }

    #[test]
    fn ticket_text_roundtrip_and_prefix() {
        let t = ticket();
        let s = t.to_text();
        assert!(s.starts_with("mesh1"));
        assert_eq!(s, s.to_lowercase());
        assert_eq!(InviteTicket::from_text(&s).unwrap(), t);
    }

    #[test]
    fn ticket_crc_failure() {
        let mut s = ticket().to_text();
        flip_last_char(&mut s);
        assert!(matches!(
            InviteTicket::from_text(&s),
            Err(ProtoError::Ticket(_))
        ));
    }

    #[test]
    fn ticket_bad_prefix_and_garbage() {
        assert!(matches!(
            InviteTicket::from_text("nope"),
            Err(ProtoError::Ticket(_))
        ));
        assert!(matches!(
            InviteTicket::from_text("mesh1!!"),
            Err(ProtoError::Ticket(_))
        ));
        assert!(matches!(
            InviteTicket::from_text("mesh1"),
            Err(ProtoError::Ticket(_))
        ));
    }
}
