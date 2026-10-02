//! Reads a certificate's `notAfter` straight from its DER encoding (enough
//! X.509 to find one field, without a certificate-parsing dependency).

use jiff::{Timestamp, civil::DateTime, tz::TimeZone};

/// One DER element: its tag, its contents, and what follows it.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let length = rest[..count]
            .iter()
            .fold(0usize, |n, b| (n << 8) | usize::from(*b));
        (length, &rest[count..])
    };
    if rest.len() < length {
        return None;
    }
    Some((tag, &rest[..length], &rest[length..]))
}

const SEQUENCE: u8 = 0x30;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
const VERSION: u8 = 0xa0;

/// When the certificate `der` expires.
pub(crate) fn not_after(der: &[u8]) -> Option<Timestamp> {
    let (SEQUENCE, certificate, _) = element(der)? else {
        return None;
    };
    let (SEQUENCE, tbs, _) = element(certificate)? else {
        return None;
    };
    let mut fields = tbs;
    if fields.first() == Some(&VERSION) {
        fields = element(fields)?.2;
    }
    // serialNumber, signature, issuer
    for _ in 0..3 {
        fields = element(fields)?.2;
    }
    let (SEQUENCE, validity, _) = element(fields)? else {
        return None;
    };
    let (_, _, rest) = element(validity)?; // notBefore
    let (tag, value, _) = element(rest)?;
    let text = std::str::from_utf8(value).ok()?.strip_suffix('Z')?;
    let full = match tag {
        UTC_TIME if text.len() == 12 => {
            let year: u32 = text[..2].parse().ok()?;
            let century = if year < 50 { "20" } else { "19" };
            format!("{century}{text}")
        }
        GENERALIZED_TIME if text.len() == 14 => text.to_owned(),
        _ => return None,
    };
    let civil = DateTime::strptime("%Y%m%d%H%M%S", &full).ok()?;
    civil
        .to_zoned(TimeZone::UTC)
        .ok()
        .map(|zoned| zoned.timestamp())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use pretty_assertions::assert_eq;

    use super::*;

    fn cert(year: i32, month: u8, day: u8) -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(vec!["example.test".to_owned()]).unwrap();
        params.not_after = rcgen::date_time_ymd(year, month, day);
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().der().to_vec()
    }

    #[test]
    fn reads_utc_time() {
        let der = cert(2031, 3, 1);
        assert_eq!(
            not_after(&der),
            Some("2031-03-01T00:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn reads_generalized_time() {
        let der = cert(2051, 7, 9);
        assert_eq!(
            not_after(&der),
            Some("2051-07-09T00:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(not_after(b"not a certificate"), None);
        assert_eq!(not_after(&[]), None);
        assert_eq!(not_after(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]), None);
    }
}
