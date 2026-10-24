//! Just enough X.509 to answer what ca-certificates' bundle script asks
//! `openssl x509` about each certificate: has it expired (`-checkend 0`),
//! is it an SSL server CA (`-purpose`), its SHA-256 fingerprint and the
//! SHA-256 of its public key (`-pubkey | openssl pkey -outform der`). Each
//! answer follows LibreSSL, the macOS `/usr/bin/openssl`; a certificate it
//! cannot read gets no answers, as openssl's failure skips it.

/// What the bundle script learns about one PEM certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Facts {
    /// `openssl x509 -checkend 0` succeeds.
    pub(super) unexpired: bool,
    /// `openssl x509 -purpose` says "SSL server CA : Yes".
    pub(super) ssl_server_ca: bool,
    /// The line `openssl x509 -fingerprint -sha256` prints.
    pub(super) fingerprint: String,
    /// Lower-case hex SHA-256 of the DER public key, as the script's
    /// `-pubkey | pkey -pubin -outform der | dgst -sha256` prints it.
    pub(super) spki_sha256: String,
}

/// Read one PEM certificate as the script's openssl calls would; `None` when
/// openssl would fail on it.
pub(super) fn inspect(pem: &[u8], now: chrono::DateTime<chrono::Utc>) -> Option<Facts> {
    use sha2::{Digest, Sha256};

    let der = pem_body(pem)?;
    let cert = Certificate::parse(&der)?;
    let fingerprint = Sha256::digest(&der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    Some(Facts {
        unexpired: cert.not_after.is_some_and(|end| end >= now.naive_utc()),
        ssl_server_ca: cert.is_ssl_server_ca(),
        fingerprint: format!("SHA256 Fingerprint={fingerprint}"),
        spki_sha256: Sha256::digest(cert.spki)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}

/// The DER inside the first `CERTIFICATE` block.
fn pem_body(pem: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(pem).ok()?;
    let start = text.find("-----BEGIN CERTIFICATE-----")? + "-----BEGIN CERTIFICATE-----".len();
    let end = start + text[start..].find("-----END CERTIFICATE-----")?;
    base64(&text[start..end])
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let digits: Vec<u8> = text.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let digits = digits
        .strip_suffix(b"==")
        .or_else(|| digits.strip_suffix(b"="))
        .unwrap_or(&digits);
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in digits {
        acc = (acc << 6) | u32::from(value(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// One DER element: tag, contents, and the whole encoding.
struct Tlv<'a> {
    tag: u8,
    contents: &'a [u8],
    whole: &'a [u8],
}

/// Reads DER elements one after another.
struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    fn next(&mut self) -> Option<Tlv<'a>> {
        let data = self.0;
        let tag = *data.first()?;
        let first = *data.get(1)?;
        let (len, header) = if first < 0x80 {
            (usize::from(first), 2)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 {
                return None;
            }
            let len = data
                .get(2..2 + count)?
                .iter()
                .fold(0usize, |len, byte| (len << 8) | usize::from(*byte));
            (len, 2 + count)
        };
        let end = header.checked_add(len)?;
        let whole = data.get(..end)?;
        self.0 = &data[end..];
        Some(Tlv {
            tag,
            contents: &whole[header..],
            whole,
        })
    }

    /// The next element if it has `tag`.
    fn next_if(&mut self, tag: u8) -> Option<Tlv<'a>> {
        (self.0.first() == Some(&tag))
            .then(|| self.next())
            .flatten()
    }

    fn expect(&mut self, tag: u8) -> Option<Tlv<'a>> {
        self.next().filter(|tlv| tlv.tag == tag)
    }
}

const SEQUENCE: u8 = 0x30;
const OID: u8 = 0x06;
const OCTET_STRING: u8 = 0x04;
const BIT_STRING: u8 = 0x03;
const BOOLEAN: u8 = 0x01;
const INTEGER: u8 = 0x02;

const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
const KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
const EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
const SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1d, 0x0e];
const AUTHORITY_KEY_ID: &[u8] = &[0x55, 0x1d, 0x23];
const NS_CERT_TYPE: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x86, 0xf8, 0x42, 0x01, 0x01];
const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
const NS_SGC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x86, 0xf8, 0x42, 0x04, 0x01];
const MS_SGC: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x0a, 0x03, 0x03];

/// `keyUsage` keyCertSign and the Netscape cert type bits, as LibreSSL
/// keeps them.
const KU_KEY_CERT_SIGN: u16 = 0x0004;
const NS_SSL_CA: u8 = 0x04;
const NS_ANY_CA: u8 = 0x07;

/// The parts of a certificate the purpose and expiry checks look at.
#[derive(Default)]
struct Certificate<'a> {
    v1: bool,
    serial: &'a [u8],
    issuer: &'a [u8],
    subject: &'a [u8],
    spki: &'a [u8],
    /// `None` when LibreSSL would not read the time, which `-checkend`
    /// takes as expired.
    not_after: Option<chrono::NaiveDateTime>,
    ca: Option<bool>,
    key_usage: Option<u16>,
    ext_key_usage: Option<(bool, bool)>,
    ns_cert_type: Option<u8>,
    subject_key_id: Option<&'a [u8]>,
    authority_key_id: Option<&'a [u8]>,
    authority_serial: Option<&'a [u8]>,
}

impl<'a> Certificate<'a> {
    fn parse(der: &'a [u8]) -> Option<Self> {
        let mut outer = Der(Der(der).expect(SEQUENCE)?.contents);
        let mut tbs = Der(outer.expect(SEQUENCE)?.contents);
        outer.expect(SEQUENCE)?;
        outer.expect(BIT_STRING)?;

        let mut cert = Certificate::default();
        let version = match tbs.next_if(0xa0) {
            Some(explicit) => {
                let int = Der(explicit.contents).expect(INTEGER)?;
                int.contents
                    .iter()
                    .fold(0u64, |v, b| (v << 8) | u64::from(*b))
            }
            None => 0,
        };
        cert.v1 = version == 0;
        cert.serial = tbs.expect(INTEGER)?.contents;
        tbs.expect(SEQUENCE)?;
        cert.issuer = tbs.expect(SEQUENCE)?.whole;
        let mut validity = Der(tbs.expect(SEQUENCE)?.contents);
        validity.next()?;
        cert.not_after = time(&validity.next()?);
        cert.subject = tbs.expect(SEQUENCE)?.whole;
        cert.spki = tbs.expect(SEQUENCE)?.whole;
        tbs.next_if(0x81);
        tbs.next_if(0xa1);
        tbs.next_if(0x82);
        tbs.next_if(0xa2);
        if let Some(explicit) = tbs.next_if(0xa3) {
            let mut extensions = Der(Der(explicit.contents).expect(SEQUENCE)?.contents);
            while let Some(extension) = extensions.next() {
                cert.extension(extension.contents)?;
            }
        }
        Some(cert)
    }

    fn extension(&mut self, der: &'a [u8]) -> Option<()> {
        let mut fields = Der(der);
        let oid = fields.expect(OID)?.contents;
        fields.next_if(BOOLEAN);
        let value = fields.expect(OCTET_STRING)?.contents;
        match oid {
            BASIC_CONSTRAINTS => {
                let mut inner = Der(Der(value).expect(SEQUENCE)?.contents);
                let ca = inner
                    .next_if(BOOLEAN)
                    .is_some_and(|b| b.contents.first().is_some_and(|v| *v != 0));
                self.ca = Some(ca);
            }
            KEY_USAGE => {
                let bits = Der(value).expect(BIT_STRING)?.contents;
                let low = u16::from(*bits.get(1).unwrap_or(&0));
                let high = u16::from(*bits.get(2).unwrap_or(&0));
                self.key_usage = Some(low | (high << 8));
            }
            EXT_KEY_USAGE => {
                let mut purposes = Der(Der(value).expect(SEQUENCE)?.contents);
                let (mut server, mut sgc) = (false, false);
                while let Some(purpose) = purposes.next() {
                    match purpose.contents {
                        SERVER_AUTH => server = true,
                        NS_SGC | MS_SGC => sgc = true,
                        _ => {}
                    }
                }
                self.ext_key_usage = Some((server, sgc));
            }
            NS_CERT_TYPE => {
                let bits = Der(value).expect(BIT_STRING)?.contents;
                self.ns_cert_type = Some(*bits.get(1).unwrap_or(&0));
            }
            SUBJECT_KEY_ID => self.subject_key_id = Some(Der(value).expect(OCTET_STRING)?.contents),
            AUTHORITY_KEY_ID => {
                let mut fields = Der(Der(value).expect(SEQUENCE)?.contents);
                while let Some(field) = fields.next() {
                    match field.tag {
                        0x80 => self.authority_key_id = Some(field.contents),
                        0x82 => self.authority_serial = Some(field.contents),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Some(())
    }

    /// LibreSSL's `ku_reject`: a key usage that leaves out `usage`.
    fn ku_reject(&self, usage: u16) -> bool {
        self.key_usage.is_some_and(|ku| ku & usage == 0)
    }

    /// `EXFLAG_SS`: issued by itself, its authority key id (if any) naming
    /// its own key and serial, and allowed to sign certificates.
    fn self_signed(&self) -> bool {
        let akid_matches = self
            .authority_key_id
            .zip(self.subject_key_id)
            .is_none_or(|(akid, skid)| akid == skid)
            && self
                .authority_serial
                .is_none_or(|serial| serial == self.serial);
        self.issuer == self.subject && akid_matches && !self.ku_reject(KU_KEY_CERT_SIGN)
    }

    /// LibreSSL's `check_ca`.
    fn check_ca(&self) -> u8 {
        if self.ku_reject(KU_KEY_CERT_SIGN) {
            return 0;
        }
        match self.ca {
            Some(true) => 1,
            Some(false) if self.v1 && self.self_signed() => 3,
            Some(false) => 0,
            None if self.v1 && self.self_signed() => 3,
            None if self.key_usage.is_some() => 4,
            None if self.ns_cert_type.is_some_and(|ns| ns & NS_ANY_CA != 0) => 5,
            None => 0,
        }
    }

    /// `X509_check_purpose(x, X509_PURPOSE_SSL_SERVER, 1)`: the extended key
    /// usage allows a server (or SGC), and `check_ssl_ca` holds.
    fn is_ssl_server_ca(&self) -> bool {
        if self
            .ext_key_usage
            .is_some_and(|(server, sgc)| !server && !sgc)
        {
            return false;
        }
        match self.check_ca() {
            0 => false,
            5 => self.ns_cert_type.is_some_and(|ns| ns & NS_SSL_CA != 0),
            _ => true,
        }
    }
}

/// A certificate time as LibreSSL reads it under RFC 5280: a UTCTime
/// `YYMMDDHHMMSSZ`, or a GeneralizedTime `YYYYMMDDHHMMSSZ` only from 2050
/// on. Anything else is unreadable (Certum Trusted Network CA 2 has a 2046
/// GeneralizedTime, and `openssl x509 -checkend 0` fails on it).
fn time(tlv: &Tlv) -> Option<chrono::NaiveDateTime> {
    let text = std::str::from_utf8(tlv.contents).ok()?;
    let digits = text.strip_suffix('Z')?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let full = match (tlv.tag, digits.len()) {
        (0x17, 12) => {
            let year: u32 = digits[..2].parse().ok()?;
            format!("{}{digits}", if year < 50 { "20" } else { "19" })
        }
        (0x18, 14) if digits[..4].parse::<u32>().ok()? >= 2050 => digits.to_string(),
        _ => return None,
    };
    chrono::NaiveDateTime::parse_from_str(&full, "%Y%m%d%H%M%S").ok()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Every certificate of a PEM bundle, as the script's awk splits it.
    fn certificates(pem: &[u8]) -> Vec<Vec<u8>> {
        super::super::ca_bundle::read_certificates(pem)
    }

    /// What `/usr/bin/openssl` itself says, through the script's combined call.
    fn openssl_facts(pem: &[u8]) -> Option<Facts> {
        use std::io::Write;
        let run = |args: &[&str]| {
            let mut child = Command::new("/usr/bin/openssl")
                .args(["x509", "-inform", "pem"])
                .args(args)
                .arg("-noout")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let _ = child.stdin.take().unwrap().write_all(pem);
            child.wait_with_output().unwrap()
        };
        let printed = run(&["-purpose", "-fingerprint", "-sha256"]);
        if !printed.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&printed.stdout).into_owned();
        let mut pipeline = Command::new("/bin/sh")
            .arg("-c")
            .arg("/usr/bin/openssl x509 -inform pem -pubkey -noout | /usr/bin/openssl pkey -pubin -outform der | /usr/bin/openssl dgst -sha256 -r")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _ = pipeline.stdin.take().unwrap().write_all(pem);
        let spki = pipeline.wait_with_output().unwrap();
        Some(Facts {
            unexpired: run(&["-checkend", "0"]).status.success(),
            ssl_server_ca: text.contains("SSL server CA : Yes"),
            fingerprint: text
                .lines()
                .find(|line| line.contains(" Fingerprint="))
                .unwrap()
                .to_string(),
            spki_sha256: String::from_utf8_lossy(&spki.stdout)
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string(),
        })
    }

    #[test]
    fn agrees_with_openssl_on_every_system_and_bundled_certificate() {
        let mut pems = Vec::new();
        for keychain in [
            "/Library/Keychains/System.keychain",
            "/System/Library/Keychains/SystemRootCertificates.keychain",
        ] {
            let out = Command::new("/usr/bin/security")
                .args(["find-certificate", "-a", "-p", keychain])
                .output()
                .unwrap();
            pems.extend(certificates(&out.stdout));
        }
        pems.extend(certificates(&std::fs::read("/etc/ssl/cert.pem").unwrap()));
        assert!(pems.len() > 200, "{} certificates", pems.len());

        let now = chrono::Utc::now();
        let mismatches: Vec<String> = pems
            .iter()
            .filter_map(|pem| {
                let ours = inspect(pem, now);
                let theirs = openssl_facts(pem);
                (ours != theirs).then(|| format!("ours {ours:?} openssl {theirs:?}"))
            })
            .collect();
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }
}
