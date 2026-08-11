/// Last `max_chars` characters of `s`, on a char boundary.
pub fn tail(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        s.chars().skip(count - max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_short_strings_unchanged() {
        assert_eq!(tail("abc", 10), "abc");
    }

    #[test]
    fn tail_cuts_to_the_last_max_chars() {
        assert_eq!(tail("abcdef", 3), "def");
    }

    #[test]
    fn tail_respects_multibyte_boundaries() {
        assert_eq!(tail("héllo", 4), "éllo");
    }
}
