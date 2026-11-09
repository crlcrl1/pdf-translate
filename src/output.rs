//! Resolve file aliases before a command writes any output.
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use tempfile::Builder;

fn destination(path: &Path) -> Result<PathBuf> {
    let mut target = path.to_path_buf();
    loop {
        match fs::canonicalize(&target) {
            Ok(path) => return Ok(path),
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        // Follow a dangling final symlink as well. Other symlink cycles
        // have already been rejected by canonicalize.
        if let Ok(link) = fs::read_link(&target) {
            target = target.with_file_name(link);
            continue;
        }
        let name = target.file_name().context("output path has no filename")?;
        let parent = parent_dir(&target);
        fs::create_dir_all(parent)?;
        return Ok(fs::canonicalize(parent)?.join(name));
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn require_distinct(a: &Path, b: &Path) -> Result<()> {
    let alias = a == b || (a.try_exists()? && b.try_exists()? && same_file::is_same_file(a, b)?);
    ensure!(
        !alias,
        "input and output paths must refer to distinct files: {} and {}",
        a.display(),
        b.display()
    );
    Ok(())
}

pub fn destinations(
    input: &Path,
    primary: &Path,
    layout: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let input =
        fs::canonicalize(input).with_context(|| format!("failed to open {}", input.display()))?;
    let primary = destination(primary)?;
    require_distinct(&input, &primary)?;
    let layout = layout.map(destination).transpose()?;
    if let Some(path) = &layout {
        require_distinct(&input, path)?;
        require_distinct(&primary, path)?;
    }
    Ok((primary, layout))
}

pub fn atomic_write(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    let parent = parent_dir(path);
    fs::create_dir_all(parent)?;
    let mut builder = Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut temp = builder.tempfile_in(parent)?;
    if let Ok(metadata) = fs::metadata(path) {
        if !metadata.is_file() {
            bail!("output is not a regular file: {}", path.display());
        }
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    write(temp.as_file_mut()).with_context(|| format!("failed to write {}", path.display()))?;
    temp.as_file_mut().flush()?;
    temp.persist(path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

pub fn write_bytes(path: &Path, data: &[u8]) -> Result<()> {
    atomic_write(path, |file| Ok(file.write_all(data)?))
}
