//! The operators of a function's source where Lua 5.4 and LuaJ differ
//! (s7-06e-design.md §3.6): `/`, `//`, `%` and a unary minus, read from
//! its tokens, strings and comments skipped. A hint names a candidate
//! cause; the developer settles a finding against the LuaJ table.

/// The operators in Lua source `text`, in a fixed order, each once.
pub fn operators(text: &str) -> Vec<&'static str> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut found = [false; 4];
    // Whether the last significant token ends an operand, so a `-` after
    // it is binary.
    let mut operand = false;
    let long_bracket = |at: usize| -> Option<(usize, usize)> {
        // `[`, `=`*, `[`: the level and the opening's end.
        if b.get(at) != Some(&b'[') {
            return None;
        }
        let mut j = at + 1;
        while b.get(j) == Some(&b'=') {
            j += 1;
        }
        (b.get(j) == Some(&b'[')).then_some((j - at - 1, j + 1))
    };
    let close = |from: usize, level: usize| -> usize {
        let mut j = from;
        while j < b.len() {
            if b[j] == b']' {
                let mut k = j + 1;
                let mut n = 0;
                while b.get(k) == Some(&b'=') {
                    n += 1;
                    k += 1;
                }
                if n == level && b.get(k) == Some(&b']') {
                    return k + 1;
                }
            }
            j += 1;
        }
        b.len()
    };
    while i < b.len() {
        let c = b[i];
        match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = match long_bracket(i + 2) {
                    Some((level, end)) => close(end, level),
                    None => b[i..]
                        .iter()
                        .position(|&x| x == b'\n')
                        .map_or(b.len(), |p| i + p),
                };
            }
            b'"' | b'\'' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != c {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                i = j + 1;
                operand = true;
            }
            b'[' if long_bracket(i).is_some() => {
                let (level, end) = long_bracket(i).unwrap_or((0, i + 1));
                i = close(end, level);
                operand = true;
            }
            b'/' => {
                if b.get(i + 1) == Some(&b'/') {
                    found[0] = true;
                    i += 2;
                } else {
                    found[1] = true;
                    i += 1;
                }
                operand = false;
            }
            b'%' => {
                found[2] = true;
                i += 1;
                operand = false;
            }
            b'-' => {
                if !operand {
                    found[3] = true;
                }
                i += 1;
                operand = false;
            }
            b')' | b']' | b'}' => {
                i += 1;
                operand = true;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.')
                {
                    i += 1;
                }
                let word = &text[start..i];
                operand = !matches!(
                    word,
                    "return"
                        | "and"
                        | "or"
                        | "not"
                        | "then"
                        | "do"
                        | "else"
                        | "elseif"
                        | "in"
                        | "local"
                        | "if"
                        | "while"
                        | "until"
                );
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                i += 1;
                operand = false;
            }
        }
    }
    ["//", "/", "%", "unary -"]
        .into_iter()
        .zip(found)
        .filter_map(|(o, f)| f.then_some(o))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::operators;

    #[test]
    fn strings_and_comments_name_no_operator() {
        assert!(operators("return \"a/b%c-d\" -- x / y % -z").is_empty());
        assert!(operators("return [[a/b]] --[[ - / ]] .. 'x'").is_empty());
        assert_eq!(operators("return a - b"), Vec::<&str>::new());
        assert_eq!(operators("return 1 / (-x)"), ["/", "unary -"]);
        assert_eq!(operators("return x // 2 % 3"), ["//", "%"]);
        assert_eq!(operators("return -x"), ["unary -"]);
    }
}
