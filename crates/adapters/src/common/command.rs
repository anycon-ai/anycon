//! `AdapterConfig.command` 的解析：可以是绝对路径，也可以是带参数的字符串（shell-words 切分）。
//!
//! Windows 上 `\` 是路径分隔符而不是转义（`C:\Users\me\.local\bin\claude.exe`），改用 [`split_windows`]：按空白切分，
//! 双引号或单引号括起来的部分是一个词，反斜杠原样保留。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    /// 程序名里带路径分隔符（绝对或相对路径）就不去 PATH 里找。Windows 上 `\` 也是分隔符。
    pub fn is_path(&self) -> bool {
        self.program.contains('/') || (cfg!(windows) && self.program.contains('\\'))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandParseError {
    #[error("command is empty")]
    Empty,
    #[error("invalid command quoting: {0}")]
    Quoting(String),
}

/// 用 shell-words 切分（Windows：[`split_windows`]）。空串 / 全空白 = `Empty`；引号不闭合 = `Quoting`。
pub fn parse_command(s: &str) -> Result<CommandSpec, CommandParseError> {
    let words = if cfg!(windows) {
        split_windows(s)?
    } else {
        shell_words::split(s).map_err(|e| CommandParseError::Quoting(e.to_string()))?
    };
    let mut it = words.into_iter();
    let program = it.next().ok_or(CommandParseError::Empty)?;
    Ok(CommandSpec { program, args: it.collect() })
}

/// Windows 写法的命令行切分：空白分隔；`"…"` 或 `'…'` 里的内容（含空白）属于同一个词，引号本身去掉；反斜杠不转义。
pub fn split_windows(s: &str) -> Result<Vec<String>, CommandParseError> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if let Some(q) = quote {
        return Err(CommandParseError::Quoting(format!("missing closing {q}")));
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_program_and_args() {
        let c = parse_command("opencode acp --flag 'a b'").unwrap();
        assert_eq!(c.program, "opencode");
        assert_eq!(c.args, vec!["acp", "--flag", "a b"]);
        assert!(!c.is_path());
    }

    #[test]
    fn absolute_path_with_spaces_quoted() {
        let c = parse_command("\"/Applications/My Tools/claude\"").unwrap();
        assert_eq!(c.program, "/Applications/My Tools/claude");
        assert!(c.args.is_empty());
        assert!(c.is_path());
    }

    #[test]
    fn windows_paths_keep_their_backslashes() {
        assert_eq!(
            split_windows(r#""C:\Program Files\Claude\claude.exe" --flag 'a b' x\y"#).unwrap(),
            vec![r"C:\Program Files\Claude\claude.exe", "--flag", "a b", r"x\y"]
        );
        assert_eq!(
            split_windows(r"C:\Users\me\.local\bin\claude.exe").unwrap(),
            vec![r"C:\Users\me\.local\bin\claude.exe"]
        );
        assert_eq!(split_windows(r#"a""b"#).unwrap(), vec!["ab"]);
        assert_eq!(split_windows(r#""""#).unwrap(), vec![""]);
        assert!(split_windows("   ").unwrap().is_empty());
        assert!(matches!(split_windows(r#""C:\x"#), Err(CommandParseError::Quoting(_))));
    }

    #[cfg(windows)]
    #[test]
    fn windows_absolute_path_is_a_path() {
        let c = parse_command(r"C:\Users\me\.local\bin\claude.exe --verbose").unwrap();
        assert_eq!(c.program, r"C:\Users\me\.local\bin\claude.exe");
        assert_eq!(c.args, vec!["--verbose"]);
        assert!(c.is_path());
    }

    #[test]
    fn empty_and_broken() {
        assert_eq!(parse_command("   "), Err(CommandParseError::Empty));
        assert!(matches!(parse_command("claude 'oops"), Err(CommandParseError::Quoting(_))));
    }
}
