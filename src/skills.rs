//! Claude Code skills HQ ships with. Each is written to the user's Claude skills folder when it
//! is missing, so it comes back after a Claude Code reinstall or a wiped `~/.claude`. A copy that
//! is already there is never touched: the user's own edits win.

/// (path under the Claude config folder, contents): the skill, and `/1s`, its short-form command.
const BUNDLED: [(&str, &str); 2] = [
    ("skills/one-sentence/SKILL.md", include_str!("../skills/one-sentence/SKILL.md")),
    ("commands/1s.md", include_str!("../skills/commands/1s.md")),
];

/// Writes each bundled skill that is missing. Quiet: a failure only means the skill stays missing.
pub fn install_missing() {
    let Some(dir) = crate::store::claude_config_dir() else { return };
    install_into(&dir);
}

fn install_into(config: &std::path::Path) {
    for (rel, text) in BUNDLED {
        let file = config.join(rel);
        if file.exists() {
            continue;
        }
        let written = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&file, text));
        if written.is_err() {
            eprintln!("HQ: could not install {}", file.display());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn installs_once_and_keeps_edits() {
        let dir = std::env::temp_dir().join(format!("hq-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        super::install_into(&dir);
        let file = dir.join("skills").join("one-sentence").join("SKILL.md");
        assert!(dir.join("commands").join("1s.md").is_file());
        assert!(std::fs::read_to_string(&file).unwrap().contains("name: 'one-sentence'"));
        std::fs::write(&file, "mine").unwrap();
        super::install_into(&dir);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
