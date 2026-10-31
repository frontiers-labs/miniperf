use std::borrow::Cow;

/// A function's complete spelling and its display label. Labels are never identity keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionName<'a> {
    full: &'a str,
    friendly: Cow<'a, str>,
}

impl<'a> FunctionName<'a> {
    /// Formats a raw or fully demangled Rust name, leaving other languages unchanged.
    pub fn new(full: &'a str) -> Self {
        let demangled = rustc_demangle::try_demangle(full).ok();
        let rust_name = demangled.as_ref().map(ToString::to_string);
        let name = rust_name.as_deref().unwrap_or(full);
        let legacy_hash = name.rsplit_once("::h").filter(|(_, hash)| {
            hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        let is_rust = demangled.is_some()
            || legacy_hash.is_some()
            || name
                .match_indices('[')
                .any(|(index, _)| has_disambiguator(name, index));
        let friendly = if is_rust {
            Cow::Owned(abbreviate(legacy_hash.map_or(name, |(name, _)| name)))
        } else {
            Cow::Borrowed(full)
        };
        Self { full, friendly }
    }

    /// The original input, retained for inspection, search, and lookup.
    pub fn full(&self) -> &'a str {
        self.full
    }

    /// A Rust label with generic arguments elided and compiler disambiguators hidden.
    pub fn friendly(&self) -> &str {
        &self.friendly
    }
}

fn has_disambiguator(name: &str, index: usize) -> bool {
    name[..index]
        .chars()
        .next_back()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        && name[index + 1..]
            .split_once(']')
            .is_some_and(|(hash, rest)| {
                hash.len() >= 8
                    && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && rest.starts_with("::")
            })
}

fn abbreviate(name: &str) -> String {
    let mut result = String::new();
    let mut chars = name.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '[' {
            if let Some(end) = name[index + 1..].find(']') {
                let end = index + 1 + end;
                if has_disambiguator(name, index) {
                    while chars.peek().is_some_and(|(index, _)| *index <= end) {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        if ch == '<' {
            let mut depth = 1;
            let mut end = None;
            let mut qualified = index == 0;
            for (offset, next) in name[index + 1..].char_indices() {
                match next {
                    '<' => depth += 1,
                    '>' if !name[..index + 1 + offset].ends_with('-') => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(index + 1 + offset);
                            break;
                        }
                    }
                    ' ' if depth == 1 && name[index + 1 + offset..].starts_with(" as ") => {
                        qualified = true;
                    }
                    _ => {}
                }
            }
            if let Some(end) = end {
                if !qualified {
                    result.push_str("<…>");
                    while chars.peek().is_some_and(|(index, _)| *index <= end) {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        result.push(ch);
    }
    result
}

/// Demangles a linkage name while retaining Rust hashes and crate disambiguators.
/// The complete spelling must survive recording and assembly lookup.
pub fn demangle_full(name: &str) -> String {
    match rustc_demangle::try_demangle(name) {
        Ok(name) => name.to_string(),
        Err(_) => addr2line::demangle_auto(Cow::Borrowed(name), None).into_owned(),
    }
}

/// Encodes delimiters in one folded-stack frame without changing its identity.
/// Writers must mark the folded file with [`FOLDED_NAME_ENCODING_HEADER`].
pub fn encode_folded_name(name: &str) -> String {
    name.replace('%', "%25")
        .replace(';', "%3B")
        .replace('\n', "%0A")
        .replace('\r', "%0D")
}

/// Decodes one name from a folded file marked with [`FOLDED_NAME_ENCODING_HEADER`].
pub fn decode_folded_name(name: &str) -> String {
    name.replace("%3B", ";")
        .replace("%0A", "\n")
        .replace("%0D", "\r")
        .replace("%25", "%")
}

/// Comment marking folded files whose individual names use percent-encoded delimiters.
pub const FOLDED_NAME_ENCODING_HEADER: &str = "# miniperf-folded-names-percent-v1";

#[cfg(test)]
mod tests {
    use super::FunctionName;

    #[test]
    fn formats_friendly_names() {
        for (input, expected) in [
            (
                "demo::work::<alloc::vec::Vec<u64>>::h0123456789abcdef",
                "demo::work::<…>",
            ),
            (
                "<demo::Item<u64> as demo::Run>::run::h0123456789abcdef",
                "<demo::Item<…> as demo::Run>::run",
            ),
            (
                "demo::work::{{closure}}::h0123456789abcdef",
                "demo::work::{{closure}}",
            ),
            (
                "demo::work::<fn() -> u64>::h0123456789abcdef",
                "demo::work::<…>",
            ),
            (
                "<[f64] as s::Sum>::total::h0123456789abcdef",
                "<[f64] as s::Sum>::total",
            ),
            ("<[f32]>::sort_by::h0123456789abcdef", "<[f32]>::sort_by"),
            ("demo[01234567]::work", "demo::work"),
            (
                "_RNvNtCshdxFFIzI2qy_10rust_names4left4same",
                "rust_names::left::same",
            ),
            (
                "std::vector<int>::push_back(int)",
                "std::vector<int>::push_back(int)",
            ),
            ("main", "main"),
            ("λ::name", "λ::name"),
            ("broken::<thing", "broken::<thing"),
            ("a::h123", "a::h123"),
            ("[unknown]", "[unknown]"),
            ("foo[abc]::bar", "foo[abc]::bar"),
            ("[abcdefab]::bar", "[abcdefab]::bar"),
        ] {
            assert_eq!(FunctionName::new(input).friendly(), expected, "{input}");
        }
    }

    #[test]
    fn folded_names_roundtrip_without_inventing_frames() {
        let full = "demo::work::<[u64; 4], Literal%3B>\nnext\rline::h0123456789abcdef";
        let encoded = super::encode_folded_name(full);
        assert!(!encoded.contains([';', '\n', '\r']));
        assert_eq!(super::decode_folded_name(&encoded), full);
    }
}
