use std::fs;
use std::path::Path;

/// A single pin: maps an abbreviated word sequence to an expanded word sequence.
#[derive(Debug, Clone)]
pub struct Pin {
    pub abbrev: Vec<String>,
    pub expanded: Vec<String>,
}

/// Collection of pins loaded from pins.txt.
/// Supports longest-prefix-match lookups.
#[derive(Debug, Clone, Default)]
pub struct Pins {
    pub entries: Vec<Pin>,
}

impl Pins {
    pub fn load(path: &Path) -> Self {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        let mut entries = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((left, right)) = line.split_once("->") {
                let abbrev: Vec<String> = left.split_whitespace().map(String::from).collect();
                let expanded: Vec<String> = right.split_whitespace().map(String::from).collect();
                if !abbrev.is_empty() && !expanded.is_empty() {
                    entries.push(Pin { abbrev, expanded });
                }
            }
        }
        Self { entries }
    }

    /// Find the longest-prefix-matching pin for the given abbreviated words.
    /// Returns (number of abbreviated words consumed, expanded words).
    pub fn longest_match(&self, words: &[&str]) -> Option<(usize, Vec<String>)> {
        let mut best: Option<(usize, Vec<String>)> = None;

        for pin in &self.entries {
            let pin_len = pin.abbrev.len();
            if pin_len > words.len() {
                continue;
            }
            let is_match = pin.abbrev.iter().zip(words.iter()).all(|(a, w)| a == w);
            if is_match && best.as_ref().is_none_or(|(len, _)| pin_len > *len) {
                best = Some((pin_len, pin.expanded.clone()));
            }
        }

        best
    }

    /// Append a new pin to the file.
    pub fn append(path: &Path, abbrev: &[&str], expanded: &[&str]) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{} -> {}", abbrev.join(" "), expanded.join(" "))?;
        Ok(())
    }

    /// Remove a pin by its abbreviated sequence.
    pub fn remove(path: &Path, abbrev: &[&str]) -> std::io::Result<bool> {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        let abbrev_str = abbrev.join(" ");
        let mut found = false;
        let mut lines: Vec<&str> = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some((left, _)) = trimmed.split_once("->")
                && left.trim() == abbrev_str
            {
                found = true;
                continue;
            }
            lines.push(line);
        }
        if found {
            let mut out = lines.join("\n");
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            fs::write(path, out)?;
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_longest_match() {
        let pins = Pins {
            entries: vec![
                Pin {
                    abbrev: vec!["g".into(), "ch".into()],
                    expanded: vec!["git".into(), "checkout".into()],
                },
                Pin {
                    abbrev: vec!["g".into()],
                    expanded: vec!["grep".into()],
                },
                Pin {
                    abbrev: vec!["tf".into()],
                    expanded: vec!["terraform".into()],
                },
            ],
        };

        // "g ch main" should match the more specific "g ch" pin
        let result = pins.longest_match(&["g", "ch", "main"]);
        assert_eq!(result, Some((2, vec!["git".into(), "checkout".into()])));

        // "g foo" should match the "g" pin
        let result = pins.longest_match(&["g", "foo"]);
        assert_eq!(result, Some((1, vec!["grep".into()])));

        // "tf apply" should match "tf" pin
        let result = pins.longest_match(&["tf", "apply"]);
        assert_eq!(result, Some((1, vec!["terraform".into()])));

        // "xyz" should match nothing
        let result = pins.longest_match(&["xyz"]);
        assert_eq!(result, None);
    }

    #[test]
    fn test_load_pins_file() {
        let dir = std::env::temp_dir().join("zsh-ios-test-pins");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("pins.txt");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "# comment line").unwrap();
        writeln!(f, "g ch -> git checkout").unwrap();
        writeln!(f, "tf -> terraform").unwrap();
        writeln!(f).unwrap();

        let pins = Pins::load(&path);
        assert_eq!(pins.entries.len(), 2);

        let result = pins.longest_match(&["g", "ch", "main"]);
        assert_eq!(result, Some((2, vec!["git".into(), "checkout".into()])));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_and_load() {
        let dir = std::env::temp_dir().join("zsh-ios-test-pins-append");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("pins.txt");

        Pins::append(&path, &["g", "ch"], &["git", "checkout"]).unwrap();
        Pins::append(&path, &["tf"], &["terraform"]).unwrap();

        let pins = Pins::load(&path);
        assert_eq!(pins.entries.len(), 2);
        assert_eq!(
            pins.longest_match(&["g", "ch", "main"]),
            Some((2, vec!["git".into(), "checkout".into()]))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_remove_pin() {
        let dir = std::env::temp_dir().join("zsh-ios-test-pins-remove");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("pins.txt");

        Pins::append(&path, &["g", "ch"], &["git", "checkout"]).unwrap();
        Pins::append(&path, &["tf"], &["terraform"]).unwrap();

        let found = Pins::remove(&path, &["g", "ch"]).unwrap();
        assert!(found);

        let pins = Pins::load(&path);
        assert_eq!(pins.entries.len(), 1);
        assert!(pins.longest_match(&["g", "ch"]).is_none());
        assert!(pins.longest_match(&["tf"]).is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_remove_nonexistent_pin() {
        let dir = std::env::temp_dir().join("zsh-ios-test-pins-remove-none");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("pins.txt");

        Pins::append(&path, &["tf"], &["terraform"]).unwrap();
        let found = Pins::remove(&path, &["xyz"]).unwrap();
        assert!(!found);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_remove_nonexistent_file() {
        let found = Pins::remove(Path::new("/tmp/zsh-ios-nonexistent-pins.txt"), &["a"]).unwrap();
        assert!(!found);
    }

    #[test]
    fn test_load_nonexistent_file() {
        let pins = Pins::load(Path::new("/tmp/zsh-ios-nonexistent-pins.txt"));
        assert!(pins.entries.is_empty());
    }

    #[test]
    fn test_longest_match_empty_words() {
        let pins = Pins {
            entries: vec![Pin {
                abbrev: vec!["g".into()],
                expanded: vec!["git".into()],
            }],
        };
        assert_eq!(pins.longest_match(&[]), None);
    }

    #[test]
    fn test_load_skips_lines_without_arrow() {
        let dir = std::env::temp_dir().join("zsh-ios-test-pins-noarrow");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("pins.txt");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "g ch -> git checkout").unwrap();
        writeln!(f, "this line has no arrow").unwrap();
        writeln!(f, "tf -> terraform").unwrap();

        let pins = Pins::load(&path);
        assert_eq!(pins.entries.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }
}
