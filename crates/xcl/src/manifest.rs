//! The few `MicrosoftGame.Config` settings XCL uses (game manifest in the game folder):
//! the persistent local storage quota and the title memory mode (the title sees the PC's
//! processors, so SMT does not matter). Plain text search –
//! the file is small XML and only these elements matter.

use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// `<PersistentLocalStorage><SizeMB>`
    pub pls_mb: Option<u64>,
    /// `<XboxAnacondaTitleMemory>` / `<XboxLockhartTitleMemory>`: "Standard" or "Advanced"
    pub anaconda_memory: Option<String>,
    pub lockhart_memory: Option<String>,
    /// `<TitleId>` (hex)
    pub title_id: Option<String>,
    /// `<Resources><Resource Language="..."/>`: declared package locales in manifest order
    pub languages: Vec<String>,
}

fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open).map(|i| from + i) {
        let after = i + open.len();
        let Some(next) = xml[after..].chars().next() else { break };
        if next == '>' || next.is_whitespace() {
            let gt = xml[after..].find('>')?;
            if !xml[after..after + gt].trim_end().ends_with('/') {
                let body = after + gt + 1;
                let end = xml[body..].find(&format!("</{name}>"))?;
                return Some(xml[body..body + end].trim());
            }
        }
        from = after;
    }
    None
}

/// Values of `attr` on every `<tag …>` element (so "<Executable" != "<ExecutableList").
pub(crate) fn xml_attrs(xml: &str, tag: &str, attr: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let key = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open).map(|i| from + i) {
        from = i + open.len();
        let Some(next) = xml[from..].chars().next() else { break };
        if !(next.is_whitespace() || next == '/' || next == '>') {
            continue;
        }
        let Some(end) = xml[from..].find('>') else { break };
        let t = &xml[from..from + end];
        // attribute must start at a whitespace boundary ("Name=" but not "DisplayName=")
        let mut pos = 0;
        while let Some(k) = t[pos..].find(&key).map(|k| pos + k) {
            if k == 0 || t[..k].ends_with(char::is_whitespace) {
                let a = k + key.len();
                if let Some(b) = t[a..].find('"') {
                    out.push(xml_unescape(&t[a..a + b]));
                }
                break;
            }
            pos = k + key.len();
        }
    }
    out
}

/// An attribute value with XML's character references replaced: the five named ones
/// ("Ratchet &amp; Clank") and numeric ones (`&#39;`, `&#x27;`). Anything else stays as written.
pub(crate) fn xml_unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest.find(';').and_then(|end| {
            let c = match &rest[1..end] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                r => {
                    let n = r.strip_prefix("#x").map(|h| u32::from_str_radix(h, 16)).or_else(|| r.strip_prefix('#').map(|d| d.parse()))?;
                    char::from_u32(n.ok()?)?
                }
            };
            Some((c, end + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// MicrosoftGame.config as text, without its comments: a commented-out `<Executable>` or
/// `<Identity>` is not the game's.
pub(crate) fn decode_game_config(raw: &[u8]) -> Option<String> {
    decode_text(raw).map(|xml| without_comments(&xml))
}

pub(crate) fn without_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i + 4..].find("-->") {
            Some(j) => rest = &rest[i + 4 + j + 3..],
            None => return out, // unterminated: the rest is comment
        }
    }
    out.push_str(rest);
    out
}

fn decode_text(raw: &[u8]) -> Option<String> {
    let (body, little_endian) = if let Some(body) = raw.strip_prefix(&[0xFF, 0xFE]) {
        (body, true)
    } else if let Some(body) = raw.strip_prefix(&[0xFE, 0xFF]) {
        (body, false)
    } else {
        return String::from_utf8(raw.to_vec()).ok();
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|b| if little_endian { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
        .collect();
    String::from_utf16(&units).ok()
}

pub fn parse(xml: &str) -> Manifest {
    let pls = element(xml, "PersistentLocalStorage").and_then(|section| element(section, "SizeMB"));
    let languages = element(xml, "Resources")
        .map(|section| {
            xml_attrs(section, "Resource", "Language")
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Manifest {
        pls_mb: pls.and_then(|v| v.parse().ok()),
        anaconda_memory: element(xml, "XboxAnacondaTitleMemory").map(str::to_string),
        lockhart_memory: element(xml, "XboxLockhartTitleMemory").map(str::to_string),
        title_id: element(xml, "TitleId").map(str::to_string),
        languages,
    }
}

/// `MicrosoftGame.Config` of a game folder (None: no manifest).
pub fn load(game_root: &Path) -> Option<Manifest> {
    let p = std::fs::read_dir(game_root).ok()?.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("MicrosoftGame.Config")))?;
    let raw = std::fs::read(p).ok()?;
    // UTF-8, or UTF-16 with its BOM; comments removed
    let text = decode_game_config(&raw)?;
    Some(parse(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_settings_xcl_uses() {
        let xml = r#"<Game configVersion="1">
  <PersistentLocalStorage>
    <SizeMB>2048</SizeMB>
  </PersistentLocalStorage>
  <VirtualMachine>
    <SMTEnabled>true</SMTEnabled>
    <XboxLockhartTitleMemory>Advanced</XboxLockhartTitleMemory>
    <XboxAnacondaTitleMemory>Advanced</XboxAnacondaTitleMemory>
  </VirtualMachine>
</Game>"#;
        let m = parse(xml);
        assert_eq!(m.pls_mb, Some(2048));
        assert_eq!((m.anaconda_memory.as_deref(), m.lockhart_memory.as_deref()), (Some("Advanced"), Some("Advanced")));
        assert_eq!(parse("<Game/>"), Manifest::default());
    }

    #[test]
    fn commented_out_settings_are_not_read() {
        let dir = std::env::temp_dir().join(format!("xcl_manifest_comments_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = r#"<Game configVersion="1">
  <!-- <PersistentLocalStorage><SizeMB>4096</SizeMB></PersistentLocalStorage> -->
  <PersistentLocalStorage><SizeMB>1024</SizeMB></PersistentLocalStorage>
  <VirtualMachine>
    <!--<XboxAnacondaTitleMemory>Advanced</XboxAnacondaTitleMemory>-->
  </VirtualMachine>
</Game>"#;
        let want = Manifest { pls_mb: Some(1024), ..Default::default() };
        std::fs::write(dir.join("MicrosoftGame.config"), xml).unwrap();
        assert_eq!(load(&dir), Some(want.clone()));
        // UTF-16 big-endian, as the launcher reads it too
        let mut raw = vec![0xFE, 0xFF];
        raw.extend(xml.encode_utf16().flat_map(u16::to_be_bytes));
        std::fs::write(dir.join("MicrosoftGame.config"), raw).unwrap();
        assert_eq!(load(&dir), Some(want));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_take_storage_size_from_another_section() {
        let xml = r#"<Game>
  <PersistentLocalStorage></PersistentLocalStorage>
  <Other><SizeMB>512</SizeMB></Other>
</Game>"#;
        assert_eq!(parse(xml).pls_mb, None);
    }

    #[test]
    fn parses_declared_resources_languages_with_comments_and_utf16() {
        let xml = r#"<Game configVersion="1">
  <Other><Resource Language="xx-XX"/></Other>
  <Resources>
    <!-- <Resource Language="de-DE"/> -->
    <Resource Language="en-US"/>
    <Resource Scale="100"/>
    <Resource Language="fr-FR" />
    <Resource Language="   "/>
    <Resource Language="ja-JP"></Resource>
  </Resources>
</Game>"#;
        assert_eq!(parse(xml).languages, vec!["de-DE", "en-US", "fr-FR", "ja-JP"]);

        let dir = std::env::temp_dir().join(format!("xcl_manifest_langs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MicrosoftGame.Config"), xml).unwrap();
        assert_eq!(load(&dir).unwrap().languages, vec!["en-US", "fr-FR", "ja-JP"]);

        // UTF-16 LE with BOM
        let mut raw = vec![0xFF, 0xFE];
        raw.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(dir.join("MicrosoftGame.Config"), raw).unwrap();
        assert_eq!(load(&dir).unwrap().languages, vec!["en-US", "fr-FR", "ja-JP"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
