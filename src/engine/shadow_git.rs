//! Shadow Git / Dotfiles Time Machine (`~/.local/share/waish/shadow_repo/`).

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::PRIVATE_DIR_MODE;

/// Outcome of a pre-edit Shadow Git snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    NewFile,
    Committed {
        commit_hash: String,
        backup_path: Option<PathBuf>,
    },
    Unchanged { backup_path: Option<PathBuf> },
}

/// Revision content retrieved from Shadow Git or `.waish.bak` fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSnapshot {
    pub content: String,
    pub source_label: String,
}

/// Internal Shadow Git repository manager for configuration backup and rollback.
#[derive(Debug, Clone)]
pub struct ShadowGit {
    repo_dir: PathBuf,
}

impl ShadowGit {
    pub fn new(repo_dir: PathBuf) -> Self {
        Self { repo_dir }
    }

    pub fn ensure_initialized(&self) -> crate::Result<()> {
        if !self.repo_dir.exists() {
            fs::create_dir_all(&self.repo_dir).map_err(|e| {
                format!(
                    "Gagal membuat direktori Shadow Git {}: {e}",
                    self.repo_dir.display()
                )
            })?;
            let _ = fs::set_permissions(
                &self.repo_dir,
                fs::Permissions::from_mode(PRIVATE_DIR_MODE),
            );
        }

        if !self.repo_dir.join(".git").exists() {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.repo_dir)
                .arg("init")
                .arg("-q")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .status()
                .map_err(|e| format!("Gagal menjalankan `git init` untuk Shadow Git: {e}"))?;

            if !status.success() {
                return Err(format!("`git init` gagal dengan status {status}").into());
            }

            self.set_local_git_config("user.name", "waish-shadow")?;
            self.set_local_git_config("user.email", "waish@localhost")?;
            self.set_local_git_config("commit.gpgsign", "false")?;
        }

        Ok(())
    }

    fn set_local_git_config(&self, key: &str, value: &str) -> crate::Result<()> {
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.repo_dir)
            .args(["config", "--local", key, value])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Gagal menyetel git config {key}: {e}"))?;

        if !status.success() {
            return Err(format!("`git config --local {key}` gagal ({status})").into());
        }
        Ok(())
    }

    pub fn mirror_relative_path(target: &Path) -> PathBuf {
        let abs = normalize_absolute_path(target);

        if let Some(home) = dirs::home_dir() {
            let norm_home = normalize_absolute_path(&home);
            if let Ok(rel) = abs.strip_prefix(&norm_home) {
                return PathBuf::from("home").join(rel);
            }
        }

        let mut stripped = PathBuf::new();
        for comp in abs.components() {
            if let Component::Normal(part) = comp {
                stripped.push(part);
            }
        }
        if stripped.as_os_str().is_empty() {
            PathBuf::from("root_file")
        } else {
            stripped
        }
    }

    pub fn instant_backup_path(target: &Path) -> PathBuf {
        let mut os_name = target.as_os_str().to_os_string();
        os_name.push(".waish.bak");
        PathBuf::from(os_name)
    }

    pub fn snapshot_before_edit(&self, target: &Path) -> crate::Result<SnapshotOutcome> {
        if !target.exists() {
            return Ok(SnapshotOutcome::NewFile);
        }

        let bak_path = Self::instant_backup_path(target);
        let backup_path = fs::copy(target, &bak_path).ok().map(|_| bak_path);

        self.ensure_initialized()?;

        let rel_path = Self::mirror_relative_path(target);
        let mirror_full = self.repo_dir.join(&rel_path);

        if let Some(parent) = mirror_full.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Gagal membuat folder mirror Shadow Git {}: {e}",
                    parent.display()
                )
            })?;
        }

        fs::copy(target, &mirror_full).map_err(|e| {
            format!(
                "Gagal menyalin snapshot {} ke Shadow Git: {e}",
                target.display()
            )
        })?;

        let add_status = Command::new("git")
            .arg("-C")
            .arg(&self.repo_dir)
            .arg("add")
            .arg("--")
            .arg(&rel_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| format!("Gagal menjalankan `git add` di Shadow Git: {e}"))?;

        if !add_status.success() {
            return Err(format!("`git add` di Shadow Git gagal ({add_status})").into());
        }

        let diff_status = Command::new("git")
            .arg("-C")
            .arg(&self.repo_dir)
            .args(["diff", "--cached", "--quiet", "--"])
            .arg(&rel_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Gagal mengecek diff Shadow Git: {e}"))?;

        if diff_status.success() {
            return Ok(SnapshotOutcome::Unchanged { backup_path });
        }

        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let commit_msg = format!(
            "Snapshot before waish update: {} [{}]",
            target.display(),
            timestamp
        );

        let commit_out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_dir)
            .args(["commit", "-q", "-m", &commit_msg])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("Gagal menjalankan `git commit` di Shadow Git: {e}"))?;

        if !commit_out.status.success() {
            let stderr = String::from_utf8_lossy(&commit_out.stderr);
            return Err(format!("`git commit` Shadow Git gagal: {}", stderr.trim()).into());
        }

        let hash_out = Command::new("git")
            .arg("-C")
            .arg(&self.repo_dir)
            .args(["rev-parse", "--short", "HEAD"])
            .stdin(Stdio::null())
            .output()
            .ok();

        let commit_hash = hash_out
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "HEAD".to_string());

        Ok(SnapshotOutcome::Committed {
            commit_hash,
            backup_path,
        })
    }

    pub fn fetch_revision(
        &self,
        target: &Path,
        steps_back: u32,
    ) -> crate::Result<RevisionSnapshot> {
        let steps = steps_back.max(1) as usize;
        let rel_path = Self::mirror_relative_path(target);

        if self.repo_dir.join(".git").exists() {
            let log_out = Command::new("git")
                .arg("-C")
                .arg(&self.repo_dir)
                .args(["log", "--format=%H", "-n", &steps.to_string(), "--"])
                .arg(&rel_path)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("Gagal membaca riwayat Shadow Git: {e}"))?;

            if log_out.status.success() {
                let commits: Vec<String> = String::from_utf8_lossy(&log_out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();

                if let Some(chosen_commit) = commits.get(steps - 1).or_else(|| commits.last()) {
                    let spec = format!("{chosen_commit}:{}", rel_path.to_string_lossy());
                    let show_out = Command::new("git")
                        .arg("-C")
                        .arg(&self.repo_dir)
                        .args(["show", &spec])
                        .stdin(Stdio::null())
                        .output()
                        .map_err(|e| format!("Gagal menjalankan `git show` Shadow Git: {e}"))?;

                    if show_out.status.success() {
                        let content = String::from_utf8_lossy(&show_out.stdout).into_owned();
                        let short_hash: String = chosen_commit.chars().take(7).collect();
                        return Ok(RevisionSnapshot {
                            content,
                            source_label: format!("shadow_repo commit {short_hash}"),
                        });
                    }
                }
            }
        }

        let bak_path = Self::instant_backup_path(target);
        if bak_path.is_file() {
            let content = fs::read_to_string(&bak_path).map_err(|e| {
                format!(
                    "Gagal membaca cadangan lokal {}: {e}",
                    bak_path.display()
                )
            })?;
            return Ok(RevisionSnapshot {
                content,
                source_label: format!("fallback {}", bak_path.display()),
            });
        }

        Err(format!(
            "Belum ada snapshot Shadow Git maupun cadangan .waish.bak untuk {}",
            target.display()
        )
        .into())
    }
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let raw = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };

    let mut clean = PathBuf::new();
    for comp in raw.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_home_and_system_paths_cleanly() {
        let sys_rel = ShadowGit::mirror_relative_path(Path::new("/etc/environment"));
        assert_eq!(sys_rel, PathBuf::from("etc/environment"));

        if let Some(home) = dirs::home_dir() {
            let dotfile = home.join(".config/waybar/style.css");
            let home_rel = ShadowGit::mirror_relative_path(&dotfile);
            assert_eq!(home_rel, PathBuf::from("home/.config/waybar/style.css"));
        }
    }

    #[test]
    fn snapshots_multiple_revisions_and_fetches_rollback_steps() {
        let root = env::temp_dir().join(format!("waish-shadow-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let repo_dir = root.join("shadow_repo");
        let target = root.join("waybar.css");
        let shadow = ShadowGit::new(repo_dir);

        // Versi 1 di disk
        fs::write(&target, "color: red;\n").unwrap();
        let snap1 = shadow.snapshot_before_edit(&target).unwrap();
        assert!(matches!(snap1, SnapshotOutcome::Committed { .. }));
        assert!(ShadowGit::instant_backup_path(&target).is_file());

        // Ubah ke Versi 2 di disk, lalu snapshot sebelum edit ke Versi 3
        fs::write(&target, "color: blue;\n").unwrap();
        let snap2 = shadow.snapshot_before_edit(&target).unwrap();
        assert!(matches!(snap2, SnapshotOutcome::Committed { .. }));

        // Ubah ke Versi 3 di disk
        fs::write(&target, "color: green;\n").unwrap();

        // Rollback 1 langkah -> harus dapat Versi 2
        let rev1 = shadow.fetch_revision(&target, 1).unwrap();
        assert_eq!(rev1.content, "color: blue;\n");

        // Rollback 2 langkah -> harus dapat Versi 1
        let rev2 = shadow.fetch_revision(&target, 2).unwrap();
        assert_eq!(rev2.content, "color: red;\n");

        let _ = fs::remove_dir_all(&root);
    }
}
