#[cfg(feature = "memory")]
use crate::extras::memory::{Mem, WriteMode, WriteTarget, is_safe_daily_name};
use crate::ui::slash::SlashCtx;
use crate::ui::slash::write_error;
#[cfg(feature = "memory")]
use crate::ui::slash::write_ok;
#[cfg(feature = "memory")]
use crate::ui::slash::write_result;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::process_creation::StdCommandCreationExt;

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    #[cfg(not(feature = "memory"))]
    {
        let _ = parts;
        write_error(
            ctx.renderer,
            "/memory is not available. Rebuild with:\n  cargo install --path . --debug --features memory",
        );
    }
    #[cfg(feature = "memory")]
    {
        match parts.get(1).copied() {
            None | Some("status") => handle_status(ctx),
            Some("search") => handle_search(parts, ctx),
            Some("read") => handle_read(parts, ctx),
            Some("write") => handle_write(parts, ctx),
            Some("editor") => return handle_editor(ctx),
            Some("clear") => handle_clear(parts, ctx),
            _ => {
                write_error(
                    ctx.renderer,
                    "usage: /memory [status|search|read|write|editor|clear]",
                );
            }
        }
    }
    Ok(())
}

#[cfg(feature = "memory")]
fn handle_status(ctx: &mut SlashCtx<'_>) {
    let mem = Mem::open();
    write_ok(ctx.renderer, "memory status:");

    let long_term = mem.memory_md();
    if long_term.exists() {
        match std::fs::metadata(&long_term) {
            Ok(m) => write_result(ctx.renderer, format!("  MEMORY.md: {}B", m.len())),
            Err(_) => write_result(ctx.renderer, "  MEMORY.md: exists (size unknown)"),
        }
    } else {
        write_result(ctx.renderer, "  MEMORY.md: (not created)");
    }

    let scratchpad = mem.scratchpad();
    if scratchpad.exists() {
        match std::fs::read_to_string(&scratchpad) {
            Ok(s) => {
                let open: Vec<&str> = s
                    .lines()
                    .filter(|l| {
                        let t = l.trim_start();
                        t.starts_with("- [ ]") || t.starts_with("* [ ]")
                    })
                    .collect();
                write_result(
                    ctx.renderer,
                    format!("  scratchpad: {} open item(s)", open.len()),
                );
            }
            Err(_) => write_result(ctx.renderer, "  scratchpad: exists (unreadable)"),
        }
    } else {
        write_result(ctx.renderer, "  scratchpad: (empty)");
    }

    let today = mem.daily_file(&mem.today);
    if today.exists() {
        match std::fs::read_to_string(&today) {
            Ok(s) => {
                let entries = s.lines().filter(|l| l.starts_with("### ")).count();
                write_result(ctx.renderer, format!("  today: {entries} entry(s)"));
            }
            Err(_) => write_result(ctx.renderer, "  today: exists (unreadable)"),
        }
    } else {
        write_result(ctx.renderer, "  today: (no entries)");
    }
}

#[cfg(feature = "memory")]
fn handle_search(parts: &[&str], ctx: &mut SlashCtx<'_>) {
    if parts.len() < 3 {
        write_error(ctx.renderer, "usage: /memory search <query>");
        return;
    }
    let query = parts[2..].join(" ");
    let mem = Mem::open();
    let results = mem.search(&query);
    let rendered = results.render(4000);
    write_ok(ctx.renderer, "search results:");
    for line in rendered.lines() {
        write_result(ctx.renderer, line);
    }
}

#[cfg(feature = "memory")]
fn handle_read(parts: &[&str], ctx: &mut SlashCtx<'_>) {
    handle_read_with_mem(parts, ctx.renderer, &Mem::open());
}

#[cfg(feature = "memory")]
fn handle_read_with_mem(parts: &[&str], renderer: &mut crate::ui::renderer::Renderer, mem: &Mem) {
    if parts.len() < 3 {
        write_error(renderer, "usage: /memory read <source> [YYYY-MM-DD|name]");
        write_result(
            renderer,
            "  sources: long_term, scratchpad, daily [YYYY-MM-DD], note <name>",
        );
        return;
    }
    let source = parts[2].to_lowercase();
    let path = match source.as_str() {
        "long_term" | "long" => Some(mem.memory_md()),
        "scratchpad" => Some(mem.scratchpad()),
        "daily" => {
            let date = parts.get(3).copied().unwrap_or(&mem.today);
            if is_safe_daily_name(date) {
                Some(mem.daily_file(date))
            } else {
                write_error(renderer, "invalid daily date name (expected YYYY-MM-DD)");
                None
            }
        }
        "note" => {
            let name = parts.get(3);
            name.and_then(|n| mem.note_path(n))
        }
        _ => {
            write_error(renderer, format!("unknown source: {source}"));
            write_result(
                renderer,
                "  sources: long_term, scratchpad, daily [YYYY-MM-DD], note <name>",
            );
            None
        }
    };
    if let Some(p) = path {
        match std::fs::read_to_string(&p) {
            Ok(s) => {
                let capped: String = if s.len() > 4000 {
                    s.chars().take(4000).collect::<String>() + "\n…[truncated]"
                } else {
                    s
                };
                write_ok(renderer, format!("{} ({source}):", p.display()));
                for line in capped.lines() {
                    write_result(renderer, line);
                }
            }
            Err(e) => write_error(renderer, format!("read error: {e}")),
        }
    }
}

#[cfg(feature = "memory")]
fn handle_write(parts: &[&str], ctx: &mut SlashCtx<'_>) {
    if parts.len() < 4 {
        write_error(ctx.renderer, "usage: /memory write <target> <content>");
        write_result(
            ctx.renderer,
            "  targets: long_term, scratchpad, daily, note:<name>",
        );
        return;
    }
    let mem = Mem::open();
    let target_str = parts[2].to_lowercase();
    let content = parts[3..].join(" ");

    let (target, name) = if let Some(note_name) = target_str.strip_prefix("note:") {
        (WriteTarget::Note, Some(note_name))
    } else {
        match target_str.as_str() {
            "long_term" | "long" => (WriteTarget::LongTerm, None),
            "scratchpad" => (WriteTarget::Scratchpad, None),
            "daily" => (WriteTarget::Daily, None),
            _ => {
                write_error(ctx.renderer, format!("unknown target: {target_str}"));
                write_result(
                    ctx.renderer,
                    "  targets: long_term, scratchpad, daily, note:<name>",
                );
                return;
            }
        }
    };

    match mem.write(target, &content, WriteMode::Append, name) {
        Ok(msg) => write_ok(ctx.renderer, msg),
        Err(e) => write_error(ctx.renderer, format!("write error: {e}")),
    }
}

#[cfg(feature = "memory")]
fn handle_editor(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mem = Mem::open();
    let path = mem.memory_md();
    write_ok(
        ctx.renderer,
        format!("opening {} in editor...", path.display()),
    );
    Err(anyhow::anyhow!("DEFER_EDITOR:{}", path.display()))
}

struct EditorTempFile {
    path: PathBuf,
}

impl Drop for EditorTempFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "failed to remove temporary memory editor file {}: {error}",
                self.path.display()
            );
        }
    }
}

pub(crate) fn edit_memory_file(path: &Path, editor: &str) -> std::io::Result<bool> {
    edit_memory_file_with_shell(Path::new("sh"), path, editor)
}

fn edit_memory_file_with_shell(shell: &Path, path: &Path, editor: &str) -> std::io::Result<bool> {
    let original = match crate::fs::open_private_file(path) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            bytes
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "memory file must have a parent directory",
        )
    })?;
    crate::fs::ensure_private_directory(parent)?;

    let temp = EditorTempFile {
        path: parent.join(format!(".memory-editor-{}.tmp", uuid::Uuid::new_v4())),
    };
    crate::fs::private_atomic_create_sync(&temp.path, &original)?;

    let status = std::process::Command::new(shell)
        .arg("-c")
        .arg(format!("{} \"$1\"", editor))
        .arg("sh")
        .arg(&temp.path)
        .status_guarded()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "editor exited unsuccessfully: {status}"
        )));
    }

    let mut edited = Vec::new();
    crate::fs::open_private_file(&temp.path)?.read_to_end(&mut edited)?;
    if edited == original {
        return Ok(false);
    }

    crate::fs::private_atomic_write_sync(path, &edited)?;
    Ok(true)
}

#[cfg(unix)]
pub(crate) fn verify_memory_editor_preservation() -> std::io::Result<()> {
    let root = std::env::temp_dir().join(format!(
        "mini-agent-memory-editor-check-{}",
        uuid::Uuid::new_v4()
    ));
    let result = verify_memory_editor_preservation_at(&root);
    let cleanup = std::fs::remove_dir_all(&root);

    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        (Ok(()), _) => Ok(()),
    }
}

#[cfg(unix)]
fn verify_memory_editor_preservation_at(root: &Path) -> std::io::Result<()> {
    let path = root.join("MEMORY.md");
    let original: &[u8] = b"memory editor preservation sentinel\n\xff";
    crate::fs::private_atomic_write_sync(&path, original)?;

    if edit_memory_file(&path, "printf 'discarded' > \"$1\"; false").is_ok() {
        return Err(std::io::Error::other(
            "failing editor unexpectedly reported success",
        ));
    }
    if std::fs::read(&path)? != original {
        return Err(std::io::Error::other(
            "failing editor changed existing memory bytes",
        ));
    }
    if std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".memory-editor-")
        })
    {
        return Err(std::io::Error::other(
            "failing editor left temporary memory content behind",
        ));
    }
    Ok(())
}

#[cfg(feature = "memory")]
fn handle_clear(parts: &[&str], ctx: &mut SlashCtx<'_>) {
    if parts.len() < 3 {
        write_error(ctx.renderer, "usage: /memory clear scratchpad|daily");
        return;
    }
    let mem = Mem::open();
    let target = parts[2].to_lowercase();
    let _path = match target.as_str() {
        "scratchpad" => Some(mem.scratchpad()),
        "daily" => Some(mem.daily_file(&mem.today)),
        _ => {
            write_error(ctx.renderer, "clear only supports: scratchpad, daily");
            None
        }
    };
    if _path.is_some() {
        match mem.write(
            if target == "scratchpad" {
                WriteTarget::Scratchpad
            } else {
                WriteTarget::Daily
            },
            "",
            WriteMode::Overwrite,
            None,
        ) {
            Ok(msg) => write_ok(ctx.renderer, msg),
            Err(e) => write_error(ctx.renderer, format!("clear error: {e}")),
        }
    }
}

#[cfg(all(test, feature = "memory", unix))]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("mini-agent-{label}-{}", uuid::Uuid::new_v4()))
            .join("MEMORY.md")
    }

    fn assert_no_editor_temp(parent: &Path) {
        let remaining = std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with(".memory-editor-"))
            .collect::<Vec<_>>();
        assert!(
            remaining.is_empty(),
            "temporary files remain: {remaining:?}"
        );
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    fn rendered_text(renderer: &crate::ui::renderer::Renderer) -> String {
        let width = 4096;
        let line_count = renderer.feed().line_count(width);
        if line_count == 0 {
            return String::new();
        }
        renderer
            .feed()
            .selected_text(width, 0, line_count - 1)
            .unwrap()
    }

    #[test]
    fn memory_read_daily_uses_requested_date_and_rejects_invalid_names() {
        let root =
            std::env::temp_dir().join(format!("mini-agent-memory-read-{}", uuid::Uuid::new_v4()));
        let mem = Mem {
            root: root.clone(),
            project: "proj".to_string(),
            today: "2026-05-25".to_string(),
        };
        std::fs::create_dir_all(mem.daily_file(&mem.today).parent().unwrap()).unwrap();
        std::fs::write(mem.daily_file("2026-05-15"), "REQUESTED DAILY CONTENT").unwrap();

        let mut valid_renderer = crate::ui::renderer::Renderer::new().unwrap();
        handle_read_with_mem(
            &["/memory", "read", "daily", "2026-05-15"],
            &mut valid_renderer,
            &mem,
        );
        let valid_output = rendered_text(&valid_renderer);
        assert!(valid_output.contains("2026-05-15.md (daily):"));
        assert!(valid_output.contains("REQUESTED DAILY CONTENT"));

        std::fs::write(mem.daily_file(&mem.today), "TODAY DAILY CONTENT").unwrap();
        let mut default_renderer = crate::ui::renderer::Renderer::new().unwrap();
        handle_read_with_mem(&["/memory", "read", "daily"], &mut default_renderer, &mem);
        let default_output = rendered_text(&default_renderer);
        assert!(default_output.contains("2026-05-25.md (daily):"));
        assert!(default_output.contains("TODAY DAILY CONTENT"));

        let escaped_path = root.join("projects/etc/passwd.md");
        std::fs::create_dir_all(escaped_path.parent().unwrap()).unwrap();
        std::fs::write(&escaped_path, "TRAVERSAL CONTENT").unwrap();
        let mut invalid_renderer = crate::ui::renderer::Renderer::new().unwrap();
        handle_read_with_mem(
            &["/memory", "read", "daily", "../../etc/passwd"],
            &mut invalid_renderer,
            &mem,
        );
        let invalid_output = rendered_text(&invalid_renderer);
        assert!(invalid_output.contains("invalid daily date name"));
        assert!(!invalid_output.contains("TRAVERSAL CONTENT"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn memory_editor_preserves_existing_content_on_unsuccessful_outcomes() {
        for (label, editor) in [
            ("nonzero", "printf 'discarded' > \"$1\"; false"),
            ("signal", "printf 'discarded' > \"$1\"; kill -TERM $$"),
        ] {
            let path = test_path(label);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original: &[u8] = b"unique original bytes\n\xff";
            std::fs::write(&path, original).unwrap();

            assert!(edit_memory_file(&path, editor).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
            assert_no_editor_temp(path.parent().unwrap());
            cleanup(&path);
        }

        let path = test_path("spawn-failure");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original: &[u8] = b"survives shell launch failure";
        std::fs::write(&path, original).unwrap();

        assert!(
            edit_memory_file_with_shell(
                Path::new("/definitely/missing/mini-agent-editor-shell"),
                &path,
                ":",
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_no_editor_temp(path.parent().unwrap());
        cleanup(&path);
    }

    #[test]
    fn memory_editor_commits_successful_changes_privately_and_cleans_temp() {
        let path = test_path("success");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"before").unwrap();

        assert!(edit_memory_file(&path, "printf 'after edit' >").unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"after edit");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_no_editor_temp(path.parent().unwrap());
        cleanup(&path);
    }

    #[test]
    fn memory_editor_noop_leaves_existing_file_untouched() {
        let path = test_path("noop");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original: &[u8] = b"same bytes";
        std::fs::write(&path, original).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();

        assert!(!edit_memory_file(&path, ":").unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_no_editor_temp(path.parent().unwrap());
        cleanup(&path);
    }

    #[test]
    fn memory_editor_safely_creates_absent_file_after_successful_edit() {
        let path = test_path("create");

        assert!(!edit_memory_file(&path, ":").unwrap());
        assert!(
            !path.exists(),
            "an unchanged empty edit must preserve file absence"
        );
        assert_no_editor_temp(path.parent().unwrap());

        assert!(edit_memory_file(&path, "printf 'new memory' >").unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"new memory");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_no_editor_temp(path.parent().unwrap());
        cleanup(&path);
    }

    #[test]
    fn memory_editor_rejects_symlink_target_without_touching_referent() {
        let path = test_path("symlink");
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let referent = parent.join("outside.md");
        std::fs::write(&referent, b"referent bytes").unwrap();
        std::os::unix::fs::symlink(&referent, &path).unwrap();

        assert!(edit_memory_file(&path, "printf 'bad' >").is_err());
        assert_eq!(std::fs::read(&referent).unwrap(), b"referent bytes");
        assert_no_editor_temp(parent);
        cleanup(&path);
    }
}
