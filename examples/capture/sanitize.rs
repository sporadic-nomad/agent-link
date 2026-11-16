//! The fixture sanitiser: rewrites the user's home path and names before anything is stored.

/// Replaces the user's home path with `/home/u`, then the home directory's own name and the
/// login name with `u`. The two names can differ (`/Users/jdoe` for login `john`).
#[derive(Debug, Clone)]
pub struct Sanitizer {
    /// `(needle, replacement)`, applied in this order.
    rules: Vec<(String, &'static str)>,
}

impl Sanitizer {
    /// Shortest name that is replaced; a shorter one would rewrite unrelated text. A shorter
    /// login is an error, a shorter home directory name is left alone.
    pub const MIN_NAME: usize = 3;

    /// Builds a sanitiser; refuses values that would corrupt the stream.
    pub fn new(home: &str, login: &str) -> Result<Self, String> {
        let home = home.trim_end_matches(['/', '\\']);
        if home.is_empty() {
            return Err("--sanitize: the home directory is empty or `/`".into());
        }
        if login.chars().count() < Self::MIN_NAME {
            return Err(format!(
                "--sanitize: login name {login:?} is shorter than {} characters; replacing it would rewrite unrelated text",
                Self::MIN_NAME
            ));
        }
        let mut names = vec![login.to_string()];
        let folder = home.rsplit(['/', '\\']).next().unwrap_or("");
        if folder.chars().count() >= Self::MIN_NAME && folder != login {
            names.push(folder.to_string());
        }
        // Longest first: a name containing the other is replaced whole.
        names.sort_by_key(|n| std::cmp::Reverse(n.len()));
        let mut rules = vec![(home.to_string(), "/home/u")];
        rules.extend(names.into_iter().map(|n| (n, "u")));
        Ok(Self { rules })
    }

    /// Whether `bytes` still contain the home path or one of the names (as when escape
    /// sequences split them in the raw stream, or the screen joins them back).
    pub fn leaks(&self, bytes: &[u8]) -> bool {
        self.rules.iter().any(|(needle, _)| {
            let needle = needle.as_bytes();
            bytes.windows(needle.len()).any(|w| w == needle)
        })
    }

    /// Sanitises raw bytes.
    pub fn apply_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        self.rules
            .iter()
            .fold(bytes.to_vec(), |acc, (needle, with)| {
                replace_all(&acc, needle.as_bytes(), with.as_bytes())
            })
    }

    /// Sanitises text.
    pub fn apply_str(&self, s: &str) -> String {
        self.rules
            .iter()
            .fold(s.to_string(), |acc, (needle, with)| {
                acc.replace(needle, with)
            })
    }

    /// Maps byte `offset` of the raw stream `raw` to the matching offset in
    /// `apply_bytes(raw)`; an offset inside a replaced name maps to the start of its replacement.
    pub fn map_offset(&self, raw: &[u8], offset: usize) -> usize {
        let (mut stage, mut offset) = (raw.to_vec(), offset);
        for (needle, with) in &self.rules {
            offset = map_through(&stage, needle.as_bytes(), with.as_bytes(), offset);
            stage = replace_all(&stage, needle.as_bytes(), with.as_bytes());
        }
        offset
    }
}

/// Replaces every non-overlapping occurrence of `needle` in `hay`.
fn replace_all(hay: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// Where byte `offset` of `hay` lands after `replace_all(hay, needle, with)`; an offset inside
/// a replaced occurrence maps to the start of its replacement.
fn map_through(hay: &[u8], needle: &[u8], with: &[u8], offset: usize) -> usize {
    if needle.is_empty() {
        return offset;
    }
    let (mut i, mut out) = (0, 0);
    while i < offset.min(hay.len()) {
        if hay[i..].starts_with(needle) {
            if offset < i + needle.len() {
                return out;
            }
            out += with.len();
            i += needle.len();
        } else {
            out += 1;
            i += 1;
        }
    }
    out + offset.saturating_sub(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_home_then_login() {
        let s = Sanitizer::new("/Users/jdoe/", "jdoe").unwrap();
        assert_eq!(
            s.apply_bytes(b"cd /Users/jdoe/src; whoami: jdoe"),
            b"cd /home/u/src; whoami: u"
        );
        assert_eq!(s.apply_str("~ jdoe /Users/jdoe"), "~ u /home/u");
        assert!(s.leaks(b"x jdoe"));
        assert!(!s.leaks(&s.apply_bytes(b"/Users/jdoe jdoe")));
        assert!(Sanitizer::new("/", "jdoe").is_err());
        assert!(Sanitizer::new("/Users/ab", "ab").is_err());
    }

    #[test]
    fn home_folder_name_differs_from_login() {
        let s = Sanitizer::new("/Users/schicki", "jdoe").unwrap();
        assert_eq!(
            s.apply_str("/Users/schicki/x schicki@host jdoe /tmp/schicki-1"),
            "/home/u/x u@host u /tmp/u-1"
        );
        assert!(s.leaks(b"prompt schicki $"));
        assert!(!s.leaks(&s.apply_bytes(b"schicki jdoe /Users/schicki")));
        // A name containing the other is replaced whole, whichever is longer.
        let s = Sanitizer::new("/home/jdoe2", "jdoe").unwrap();
        assert_eq!(s.apply_str("jdoe2 jdoe"), "u u");
        let s = Sanitizer::new("/home/jdo", "jdoe2").unwrap();
        assert_eq!(s.apply_str("jdoe2 jdo"), "u u");
        // A home directory name under three characters is left alone.
        let s = Sanitizer::new(r"C:\Users\ab", "jdoe").unwrap();
        assert_eq!(s.apply_str(r"C:\Users\ab\x ab jdoe"), "/home/u\\x ab u");
        assert!(!s.leaks(b"ab"));
    }

    #[test]
    fn maps_offsets_into_the_stored_stream() {
        let s = Sanitizer::new("/Users/jdoe", "john").unwrap();
        let raw: &[u8] = b"ab /Users/jdoe/x jdoe y john z";
        let stored = s.apply_bytes(raw);
        assert_eq!(stored, b"ab /home/u/x u y u z");
        for (i, &b) in raw.iter().enumerate() {
            if matches!(b, b'a' | b'b' | b'x' | b'y' | b'z') {
                assert_eq!(stored[s.map_offset(raw, i)], b, "offset {i}");
            }
        }
        assert_eq!(s.map_offset(raw, raw.len()), stored.len());
        let inside_home = 3 + "/Users/j".len();
        assert_eq!(s.map_offset(raw, inside_home), 3);
        let inside_folder = raw.len() - "oe y john z".len();
        assert_eq!(stored[s.map_offset(raw, inside_folder)], b'u');
    }
}
