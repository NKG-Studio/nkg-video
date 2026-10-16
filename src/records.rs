use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

pub struct Records {
    pub path: PathBuf,
    names: BTreeSet<String>,
}

impl Records {
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            names: text
                .trim_start_matches('\u{feff}')
                .lines()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn toggle(&mut self, name: &str) -> io::Result<()> {
        if name.is_empty() || name.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "文件名不能为空或包含换行",
            ));
        }
        // Reserve the temporary file before reading, so concurrent writers cannot lose entries.
        let temporary = self.path.with_extension("txt.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| {
            let mut updated = Self::load(self.path.clone())?;
            if !updated.names.remove(name) {
                updated.names.insert(name.to_owned());
            }
            for name in &updated.names {
                writeln!(file, "{name}")?;
            }
            file.sync_all()?;
            Ok(updated)
        })();
        drop(file);
        let result = result.and_then(|updated| {
            fs::rename(&temporary, &self.path)?;
            Ok(updated)
        });
        match result {
            Ok(updated) => {
                *self = updated;
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&temporary);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_persist_toggle_and_preserve_on_failure() {
        let dir = std::env::temp_dir().join(format!(
            "nkg-records-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("qualified-videos.txt");
        let mut records = Records::load(path.clone()).unwrap();
        assert!(!path.exists());
        records.toggle("合格 视频.mp4").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "合格 视频.mp4\n");
        assert!(Records::load(path.clone())
            .unwrap()
            .contains("合格 视频.mp4"));

        // A second instance must preserve entries written since it loaded.
        let mut second = Records::load(path.clone()).unwrap();
        records.toggle("other.webm").unwrap();
        second.toggle("合格 视频.mp4").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "other.webm\n");
        second.toggle("other.webm").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "");

        fs::write(&path, "\u{feff}保留.mp4\r\n保留.mp4\r\n").unwrap();
        records = Records::load(path.clone()).unwrap();
        let temporary = path.with_extension("txt.tmp");
        fs::write(&temporary, "another writer").unwrap();
        assert!(records.toggle("new.mp4").is_err());
        assert!(records.contains("保留.mp4"));
        assert!(!records.contains("new.mp4"));
        assert_eq!(fs::read_to_string(&temporary).unwrap(), "another writer");
        fs::remove_file(&temporary).unwrap();
        records.toggle("new.mp4").unwrap();
        let saved = fs::read(&path).unwrap();
        assert!(records.toggle("bad\nname").is_err());
        assert_eq!(fs::read(&path).unwrap(), saved);
        fs::write(&path, [0xff]).unwrap();
        assert!(records.toggle("new.mp4").is_err());
        assert_eq!(fs::read(&path).unwrap(), [0xff]);
        assert!(records.contains("new.mp4"));
        assert!(!temporary.exists());
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
