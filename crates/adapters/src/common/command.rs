//! `AdapterConfig.command` 的解析：可以是绝对路径，也可以是带参数的字符串（shell-words 切分）。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    /// 程序名里带路径分隔符（绝对或相对路径）就不去 PATH 里找。
    pub fn is_path(&self) -> bool {
        self.program.contains('/')
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandParseError {
    #[error("command is empty")]
    Empty,
    #[error("invalid command quoting: {0}")]
    Quoting(String),
}

/// 用 shell-words 切分。空串 / 全空白 = `Empty`；引号不闭合 = `Quoting`。
pub fn parse_command(s: &str) -> Result<CommandSpec, CommandParseError> {
    let words = shell_words::split(s).map_err(|e| CommandParseError::Quoting(e.to_string()))?;
    let mut it = words.into_iter();
    let program = it.next().ok_or(CommandParseError::Empty)?;
    Ok(CommandSpec { program, args: it.collect() })
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
    fn empty_and_broken() {
        assert_eq!(parse_command("   "), Err(CommandParseError::Empty));
        assert!(matches!(parse_command("claude 'oops"), Err(CommandParseError::Quoting(_))));
    }
}
