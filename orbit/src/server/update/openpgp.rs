//! Проверка отделённой подписи `OpenPGP` заданным открытым ключом.
//!
//! Разбирается ровно то, чем подписан манифест: открытый ключ RSA версии 4 и подпись версии 4 по двоичным данным
//! (RSA PKCS#1 v1.5, SHA-256, SHA-384 или SHA-512). Всё остальное в ключе и подписи — отказ. Отпечаток ключа
//! сверяется с заданным при разборе, а подпись должна называть этот отпечаток в защищённой её части.
//! Сам RSA проверяет aws-lc-rs: крейты PGP тянут `rsa` с неисправленной уязвимостью (RUSTSEC-2023-0071).

use anyhow::{Context, bail, ensure};
use aws_lc_rs::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};
use aws_lc_rs::signature::{
    RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA384, RSA_PKCS1_2048_8192_SHA512, RsaParameters, UnparsedPublicKey,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

const TAG_SIGNATURE: u8 = 2;
const TAG_PUBLIC_KEY: u8 = 6;
const ALGORITHM_RSA: u8 = 1;
const SIGNATURE_OVER_BINARY: u8 = 0x00;
const SUBPACKET_ISSUER_FINGERPRINT: u8 = 33;

/// Открытый ключ RSA, подписи которого принимаются.
#[derive(Debug)]
pub(in crate::server) struct ReleaseKey {
    fingerprint: [u8; 20],
    modulus_len: usize,
    /// `RSAPublicKey` в DER: так его принимает aws-lc-rs.
    der: Vec<u8>,
}

impl ReleaseKey {
    /// Разбирает ключ в ASCII-броне; `fingerprint` — ожидаемый отпечаток первичного ключа, 40 шестнадцатеричных цифр.
    pub(super) fn parse(armored: &str, fingerprint: &str) -> anyhow::Result<Self> {
        let expected = parse_fingerprint(fingerprint)?;
        let data = dearmor(armored, "PUBLIC KEY BLOCK")?;
        let (tag, body, _) = packet(&data)?;
        ensure!(tag == TAG_PUBLIC_KEY, "первый пакет ключа не открытый ключ");
        let mut reader = Reader(body);
        ensure!(reader.byte()? == 4, "версия ключа не 4");
        reader.take(4)?;
        ensure!(reader.byte()? == ALGORITHM_RSA, "ключ не RSA");
        let modulus = reader.mpi()?;
        let exponent = reader.mpi()?;
        ensure!(reader.0.is_empty(), "в открытом ключе RSA лишние данные");

        let mut hashed = vec![0x99];
        hashed.extend(u16::try_from(body.len()).context("ключ слишком велик")?.to_be_bytes());
        hashed.extend(body);
        let actual: [u8; 20] = digest(&SHA1_FOR_LEGACY_USE_ONLY, &hashed)
            .as_ref()
            .try_into()
            .context("отпечаток ключа не 20 байт")?;
        ensure!(actual == expected, "отпечаток ключа не совпал с заданным");

        let mut der = der_integer(modulus);
        der.extend(der_integer(exponent));
        Ok(Self {
            fingerprint: expected,
            modulus_len: modulus.len(),
            der: der_wrap(0x30, &der),
        })
    }

    /// Проверяет, что `armored_signature` — подпись ключа под `data`.
    pub(super) fn verify(&self, data: &[u8], armored_signature: &str) -> anyhow::Result<()> {
        let raw = dearmor(armored_signature, "SIGNATURE")?;
        let (tag, body, rest) = packet(&raw)?;
        ensure!(tag == TAG_SIGNATURE && rest.is_empty(), "подпись не из одного пакета подписи");
        let mut reader = Reader(body);
        ensure!(reader.byte()? == 4, "версия подписи не 4");
        let kind = reader.byte()?;
        let algorithm = reader.byte()?;
        let hash = reader.byte()?;
        ensure!(kind == SIGNATURE_OVER_BINARY, "подпись не по двоичным данным");
        ensure!(algorithm == ALGORITHM_RSA, "подпись не RSA");
        let parameters = hash_parameters(hash)?;
        let hashed_len = usize::from(reader.u16()?);
        let hashed = reader.take(hashed_len)?;
        let unhashed_len = usize::from(reader.u16()?);
        reader.take(unhashed_len)?;
        reader.take(2)?;
        let signature = reader.mpi()?;
        ensure!(reader.0.is_empty(), "в подписи лишние данные");
        ensure!(self.names_this_key(hashed)?, "подпись не называет отпечаток ключа");

        let trailer_len = 6 + hashed_len;
        let mut message = data.to_vec();
        message.extend(&body[..trailer_len]);
        message.extend([0x04, 0xFF]);
        message.extend(u32::try_from(trailer_len).context("подпись слишком велика")?.to_be_bytes());

        ensure!(signature.len() <= self.modulus_len, "подпись длиннее модуля ключа");
        let mut padded = vec![0; self.modulus_len - signature.len()];
        padded.extend(signature);
        UnparsedPublicKey::new(parameters, &self.der)
            .verify(&message, &padded)
            .map_err(|_| anyhow::anyhow!("подпись не сошлась"))
    }

    /// Есть ли среди защищённых подпакетов отпечаток этого ключа.
    fn names_this_key(&self, mut subpackets: &[u8]) -> anyhow::Result<bool> {
        while !subpackets.is_empty() {
            let mut reader = Reader(subpackets);
            let length = match reader.byte()? {
                short @ 0..=191 => usize::from(short),
                high @ 192..=254 => (usize::from(high) - 192) * 256 + usize::from(reader.byte()?) + 192,
                255 => usize::try_from(reader.u32()?).context("подпакет слишком велик")?,
            };
            ensure!(length >= 1, "пустой подпакет подписи");
            let content = reader.take(length)?;
            if content[0] & 0x7f == SUBPACKET_ISSUER_FINGERPRINT && content.get(1) == Some(&4) && content[2..] == self.fingerprint {
                return Ok(true);
            }
            subpackets = reader.0;
        }
        Ok(false)
    }
}

fn hash_parameters(hash: u8) -> anyhow::Result<&'static RsaParameters> {
    match hash {
        8 => Ok(&RSA_PKCS1_2048_8192_SHA256),
        9 => Ok(&RSA_PKCS1_2048_8192_SHA384),
        10 => Ok(&RSA_PKCS1_2048_8192_SHA512),
        other => bail!("хеш подписи {other} не поддерживается"),
    }
}

fn parse_fingerprint(text: &str) -> anyhow::Result<[u8; 20]> {
    ensure!(
        text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "отпечаток не из 40 шестнадцатеричных цифр"
    );
    let mut bytes = [0; 20];
    for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair)?;
        *byte = u8::from_str_radix(pair, 16).context("отпечаток не шестнадцатеричный")?;
    }
    Ok(bytes)
}

/// Тело ASCII-брони с заголовком `label`: без заголовков брони и контрольной суммы, декодированное из base64.
fn dearmor(text: &str, label: &str) -> anyhow::Result<Vec<u8>> {
    let mut lines = text.lines().map(str::trim_end);
    let begin = format!("-----BEGIN PGP {label}-----");
    ensure!(lines.any(|line| line == begin), "нет начала брони {label}");
    // Заголовки брони (`Version: …`) идут до пустой строки; в броне без заголовков пустая строка тоже первая.
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        ensure!(line.contains(": "), "в броне нет пустой строки после заголовков");
    }
    let end = format!("-----END PGP {label}-----");
    let mut encoded = String::new();
    let mut closed = false;
    for line in lines {
        if line == end {
            closed = true;
            break;
        }
        if !line.starts_with('=') {
            encoded.push_str(line);
        }
    }
    ensure!(closed, "нет конца брони {label}");
    STANDARD.decode(encoded).context("тело брони не base64")
}

/// Первый пакет `OpenPGP`: тег, тело и остаток данных. Пакеты с частичной длиной не поддерживаются.
fn packet(data: &[u8]) -> anyhow::Result<(u8, &[u8], &[u8])> {
    let mut reader = Reader(data);
    let first = reader.byte()?;
    ensure!(first & 0x80 != 0, "это не пакет OpenPGP");
    let (tag, length) = if first & 0x40 != 0 {
        let length = match reader.byte()? {
            short @ 0..=191 => usize::from(short),
            high @ 192..=223 => (usize::from(high) - 192) * 256 + usize::from(reader.byte()?) + 192,
            255 => usize::try_from(reader.u32()?).context("пакет слишком велик")?,
            _ => bail!("пакет с частичной длиной не поддерживается"),
        };
        (first & 0x3f, length)
    } else {
        let length = match first & 3 {
            0 => usize::from(reader.byte()?),
            1 => usize::from(reader.u16()?),
            2 => usize::try_from(reader.u32()?).context("пакет слишком велик")?,
            _ => bail!("пакет неопределённой длины не поддерживается"),
        };
        ((first >> 2) & 0x0f, length)
    };
    let body = reader.take(length)?;
    Ok((tag, body, reader.0))
}

/// Чтение с начала среза; нехватка данных — ошибка, а не паника.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> anyhow::Result<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(count).context("данные оборваны")?;
        self.0 = tail;
        Ok(head)
    }

    fn byte(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> anyhow::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    /// Многобайтовое целое: длина в битах и значение без ведущих нулей.
    fn mpi(&mut self) -> anyhow::Result<&'a [u8]> {
        let bits = usize::from(self.u16()?);
        ensure!(bits > 0, "пустое число");
        let value = self.take(bits.div_ceil(8))?;
        ensure!(value[0] != 0, "число с ведущим нулём");
        Ok(value)
    }
}

/// Целое без знака в DER: положительное, поэтому старший бит значения закрывается нулевым байтом.
fn der_integer(value: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(value.len() + 1);
    if value.first().is_some_and(|byte| byte & 0x80 != 0) {
        content.push(0);
    }
    content.extend(value);
    der_wrap(0x02, &content)
}

/// Элемент DER с длиной до 65535 байт: ключу RSA до 8192 бит этого хватает.
fn der_wrap(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match u8::try_from(content.len()) {
        Ok(short) if short < 0x80 => out.push(short),
        Ok(short) => out.extend([0x81, short]),
        Err(_) => {
            out.push(0x82);
            out.extend(u16::try_from(content.len()).unwrap_or(u16::MAX).to_be_bytes());
        }
    }
    out.extend(content);
    out
}

#[cfg(test)]
#[path = "../../../tests/server/update/openpgp.rs"]
mod tests;
