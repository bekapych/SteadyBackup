//! Чтение `UpdateInfo.inf` из `.cfu`: исходные конфигурации и версии обновления.

use super::util::{decode_text, Version};
use anyhow::{anyhow, bail, Result};
use std::io::{BufReader, Read};
use std::path::Path;

const NIL: u32 = 0x7FFF_FFFF;
/// Оглавление, заголовки и UpdateInfo.inf небольшие; ограничение не даёт испорченному
/// контейнеру потребовать гигабайты памяти. Большие данные пакета всегда читаются потоком.
const MAX_BLOCK: usize = 64 * 1024 * 1024;

/// Исходная конфигурация, которую обновляет `.cfu`.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateSource {
    pub name: String,
    pub vendor: String,
    pub version: Version,
}

pub fn read_sources(path: &Path) -> Result<Vec<UpdateSource>> {
    let text = read_update_info(path)?;
    parse_update_info(&text)
}

fn read_update_info(path: &Path) -> Result<String> {
    let mut f = BufReader::new(std::fs::File::open(path)?);
    let mut sig = [0u8; 4];
    f.read_exact(&mut sig)?;
    let f = std::io::Cursor::new(sig).chain(f);
    let data = if sig == [0xFF, 0xFF, 0xFF, 0x7F] {
        find_entry(Stream::new(Box::new(f)), "UpdateInfo.inf")?
    } else {
        find_entry(
            Stream::new(Box::new(flate2::read::DeflateDecoder::new(f))),
            "UpdateInfo.inf",
        )?
    };
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&data);
    Ok(decode_text(data))
}

/// Поток с текущей позицией; контейнер читается только вперёд.
struct Stream {
    r: Box<dyn Read>,
    pos: u64,
}

impl Stream {
    fn new(r: Box<dyn Read>) -> Stream {
        Stream { r, pos: 0 }
    }

    fn skip_to(&mut self, pos: u64) -> Result<()> {
        if pos < self.pos {
            bail!("контейнер: блок {pos} уже пройден (позиция {})", self.pos);
        }
        let n = pos - self.pos;
        let got = std::io::copy(&mut (&mut self.r).take(n), &mut std::io::sink())?;
        self.pos += got;
        if got < n {
            bail!("контейнер обрезан");
        }
        Ok(())
    }

    fn read(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut b = vec![0u8; n];
        self.r
            .read_exact(&mut b)
            .map_err(|_| anyhow!("контейнер обрезан"))?;
        self.pos += n as u64;
        Ok(b)
    }

    /// Блок контейнера: заголовок `\r\nXXXXXXXX XXXXXXXX XXXXXXXX \r\n` и данные.
    fn block(&mut self, off: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut cur = off;
        let mut full = None;
        loop {
            self.skip_to(cur)?;
            let h = self.read(31)?;
            if &h[..2] != b"\r\n" || &h[29..31] != b"\r\n" {
                bail!("контейнер: нет заголовка блока по смещению {cur}");
            }
            let num = |i: usize| {
                std::str::from_utf8(&h[2 + i * 9..10 + i * 9])
                    .ok()
                    .and_then(|s| u32::from_str_radix(s, 16).ok())
                    .ok_or_else(|| anyhow!("контейнер: испорчен заголовок блока"))
            };
            let total = *full.get_or_insert(num(0)? as usize);
            if total > MAX_BLOCK {
                bail!("контейнер: блок UpdateInfo.inf слишком большой");
            }
            let page = num(1)? as usize;
            let next = num(2)?;
            let remaining = total
                .checked_sub(out.len())
                .ok_or_else(|| anyhow!("контейнер: размер блока уменьшился"))?;
            let take = page.min(remaining);
            out.extend(self.read(take)?);
            if out.len() >= total || next == NIL {
                return Ok(out);
            }
            cur = next as u64;
        }
    }
}

fn find_entry(mut s: Stream, wanted: &str) -> Result<Vec<u8>> {
    let head = s.read(16)?;
    if head[..4] != [0xFF, 0xFF, 0xFF, 0x7F] {
        bail!("не контейнер 1С");
    }
    let toc = s.block(16)?;
    let mut entries: Vec<(u32, u32)> = toc
        .as_chunks::<12>()
        .0
        .iter()
        .map(|r| {
            (
                u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
                u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            )
        })
        .filter(|(ah, _)| *ah != NIL)
        .collect();
    entries.sort();
    for (ah, ad) in entries {
        if (ah as u64) < s.pos {
            continue;
        }
        let h = s.block(ah as u64)?;
        let units: Vec<u16> = h
            .get(20..)
            .unwrap_or_default()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take_while(|&u| u != 0)
            .collect();
        if String::from_utf16_lossy(&units) == wanted {
            return s.block(ad as u64);
        }
    }
    bail!("в контейнере нет {wanted}")
}

/// Элементы верхнего уровня скобочного списка `{a,"b",{c},…}`; строки — без кавычек.
fn top_items(text: &str, limit: usize) -> Vec<String> {
    let text = text.trim_start_matches('\u{FEFF}').trim_start();
    let mut chars = text.chars().peekable();
    if chars.next() != Some('{') {
        return Vec::new();
    }
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' if depth == 0 => {
                items.push(cur.trim().to_string());
                break;
            }
            '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                items.push(cur.trim().to_string());
                cur.clear();
                if items.len() >= limit {
                    break;
                }
            }
            _ => cur.push(c),
        }
    }
    items
}

/// Разбор начала `UpdateInfo.inf`: `{3,N,"Имя","Поставщик","Версия",uuid, …}`.
pub fn parse_update_info(text: &str) -> Result<Vec<UpdateSource>> {
    let head = top_items(text, 2);
    if head.len() < 2 || head[0] != "3" {
        bail!("UpdateInfo.inf: неизвестный формат");
    }
    let n: usize = head[1]
        .parse()
        .map_err(|_| anyhow!("UpdateInfo.inf: неизвестный формат"))?;
    let count = n
        .checked_mul(4)
        .and_then(|v| v.checked_add(2))
        .ok_or_else(|| anyhow!("UpdateInfo.inf: слишком много источников"))?;
    let items = top_items(text, count);
    let mut out = Vec::new();
    for q in items
        .get(2..)
        .unwrap_or_default()
        .as_chunks::<4>()
        .0
        .iter()
        .take(n)
    {
        if let Some(version) = Version::parse(&q[2]) {
            out.push(UpdateSource {
                name: q[0].clone(),
                vendor: q[1].clone(),
                version,
            });
        }
    }
    if out.is_empty() {
        bail!("UpdateInfo.inf: нет исходных версий");
    }
    Ok(out)
}
