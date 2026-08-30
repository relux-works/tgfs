use sha1::{Digest, Sha1};

const MAIN: &str = "refs/heads/main";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Advertisement {
    pub(crate) main: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtocolError {
    Truncated,
    Malformed,
    DuplicateMain,
    MissingMain,
    Capability,
    ReceiverRefusal,
}

fn oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn pkt_line(payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    let length = payload
        .len()
        .checked_add(4)
        .ok_or(ProtocolError::Malformed)?;
    if length > 65_520 {
        return Err(ProtocolError::Malformed);
    }
    let mut result = format!("{length:04x}").into_bytes();
    result.extend_from_slice(payload);
    Ok(result)
}

fn parse_lines(bytes: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
    let mut offset = 0usize;
    let mut lines = Vec::new();
    let mut flushed = false;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 4)
            .ok_or(ProtocolError::Truncated)?;
        let header = std::str::from_utf8(header).map_err(|_| ProtocolError::Malformed)?;
        let length = usize::from_str_radix(header, 16).map_err(|_| ProtocolError::Malformed)?;
        offset += 4;
        if length == 0 {
            flushed = true;
            if offset != bytes.len() {
                return Err(ProtocolError::Malformed);
            }
            break;
        }
        if length < 4 {
            return Err(ProtocolError::Malformed);
        }
        let payload_length = length - 4;
        let payload = bytes
            .get(offset..offset + payload_length)
            .ok_or(ProtocolError::Truncated)?;
        lines.push(payload.to_vec());
        offset += payload_length;
    }
    if !flushed {
        return Err(ProtocolError::Truncated);
    }
    Ok(lines)
}

impl Advertisement {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let service = pkt_line(b"# service=git-receive-pack\n")?;
        let refs = if bytes.starts_with(&service) {
            bytes
                .get(service.len()..)
                .and_then(|rest| rest.strip_prefix(b"0000"))
                .ok_or(ProtocolError::Malformed)?
        } else {
            bytes
        };
        let lines = parse_lines(refs)?;
        let mut main = None;
        let mut references = std::collections::HashSet::new();
        for (index, raw) in lines.iter().enumerate() {
            let line = std::str::from_utf8(raw).map_err(|_| ProtocolError::Malformed)?;
            let line = line.strip_suffix('\n').ok_or(ProtocolError::Malformed)?;
            let (visible, capabilities) = line.split_once('\0').unwrap_or((line, ""));
            let (object, reference) = visible.split_once(' ').ok_or(ProtocolError::Malformed)?;
            if !oid(object) || reference.is_empty() {
                return Err(ProtocolError::Malformed);
            }
            if index == 0
                && !capabilities
                    .split_ascii_whitespace()
                    .any(|cap| cap == "report-status")
            {
                return Err(ProtocolError::Capability);
            }
            if reference == MAIN && main.replace(object.to_owned()).is_some() {
                return Err(ProtocolError::DuplicateMain);
            }
            if !references.insert(reference) || (index != 0 && !capabilities.is_empty()) {
                return Err(ProtocolError::Malformed);
            }
        }
        Ok(Self {
            main: main.ok_or(ProtocolError::MissingMain)?,
        })
    }
}

pub(crate) fn fixed_update(old: &str, new: &str) -> Result<Vec<u8>, ProtocolError> {
    if !oid(old) || !oid(new) || old == new {
        return Err(ProtocolError::Malformed);
    }
    let command = format!("{old} {new} {MAIN}\0report-status\n");
    let mut body = pkt_line(command.as_bytes())?;
    body.extend_from_slice(b"0000");
    let mut pack = b"PACK\0\0\0\x02\0\0\0\0".to_vec();
    let digest = Sha1::digest(&pack);
    pack.extend_from_slice(&digest);
    body.extend_from_slice(&pack);
    Ok(body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReceiveStatus;

impl ReceiveStatus {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let lines = parse_lines(bytes)?;
        if lines.len() != 2
            || lines[0].as_slice() != b"unpack ok\n"
            || lines[1].as_slice() != b"ok refs/heads/main\n"
        {
            return Err(ProtocolError::ReceiverRefusal);
        }
        Ok(Self)
    }
}

#[cfg(test)]
pub(crate) fn encode_lines(lines: &[&[u8]]) -> Vec<u8> {
    let mut result = Vec::new();
    for line in lines {
        if let Ok(encoded) = pkt_line(line) {
            result.extend_from_slice(&encoded);
        }
    }
    result.extend_from_slice(b"0000");
    result
}
