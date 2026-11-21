use crate::ui::slash::write_usage;
use std::path::PathBuf;
use std::sync::Arc;

use crate::ui::slash::{SlashCtx, write_error, write_ok, write_result};

/// Maximum number of text files that can be /add'ed in one session.
const MAX_EXTRA_FILES: usize = 20;
/// Total byte budget across all /add'ed text files (bounded before allocation).
const MAX_AGGREGATE_EXTRA_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB

pub(crate) fn resolve_path(workspace: &std::path::Path, s: &str) -> PathBuf {
    let p = PathBuf::from(s);
    if p.is_absolute() {
        p
    } else {
        workspace.join(p)
    }
}

#[cfg(feature = "multimodal")]
fn has_pending_media(ctx: &SlashCtx<'_>) -> bool {
    !ctx.session.pending_media.is_empty()
}

#[cfg(not(feature = "multimodal"))]
fn has_pending_media(_ctx: &SlashCtx<'_>) -> bool {
    false
}

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts[0] {
        "/add" => handle_add(parts, ctx).await,
        "/drop" => handle_drop(parts, ctx).await,
        "/drop-all" => handle_drop_all(ctx).await,
        _ => Ok(()),
    }
}

async fn handle_add(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        if ctx.context.extra_files.is_empty() && !has_pending_media(ctx) {
            write_ok(ctx.renderer, "no files added (use /add <path>)");
        } else {
            write_ok(ctx.renderer, "added files:");
            // One 1-based sequence over files, then media: `/drop N` uses it.
            let mut number = 0usize;
            for f in &ctx.context.extra_files {
                number += 1;
                let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                write_result(
                    ctx.renderer,
                    format!("  {number}. {} ({size}B)", f.display()),
                );
            }
            #[cfg(feature = "multimodal")]
            for m in &ctx.session.pending_media {
                number += 1;
                let (kind, size) = match m {
                    crate::extras::multimodal::MediaAttachment::Image { data, .. } => {
                        ("image", data.len())
                    }
                    crate::extras::multimodal::MediaAttachment::Audio { data, .. } => {
                        ("audio", data.len())
                    }
                    crate::extras::multimodal::MediaAttachment::Document { data, .. } => {
                        ("document", data.len())
                    }
                };
                write_result(
                    ctx.renderer,
                    format!("  {number}. [{kind}] {} ({size}B)", m.path().display()),
                );
            }
        }
        write_usage(ctx.renderer, "usage: /add <path>  |  /drop <number|path>");
        return Ok(());
    }

    let mut rebuild = false;
    for arg in &parts[1..] {
        rebuild |= add_one(arg, ctx).await;
    }
    if rebuild {
        ctx.rebuild_agent().await;
    }
    Ok(())
}

/// Add one path; returns whether a text file joined the context (the agent
/// then needs a rebuild).
async fn add_one(arg: &str, ctx: &mut SlashCtx<'_>) -> bool {
    let path = resolve_path(ctx.workspace.root(), arg);

    if !path.exists() {
        write_error(ctx.renderer, format!("file not found: {}", path.display()));
        return false;
    }
    if !path.is_file() {
        write_error(ctx.renderer, format!("not a file: {}", path.display()));
        return false;
    }

    #[cfg(feature = "multimodal")]
    if crate::extras::multimodal::detect_media(&path).is_some() {
        let loaded =
            crate::extras::multimodal::load_attachment(&path).map_err(|error| error.to_string());
        // Oversized images are downscaled off the async executor.
        let loaded = match loaded {
            Ok(attachment) => crate::agent::runner::spawn_blocking_scoped(move || {
                let mut attachment = attachment;
                crate::extras::multimodal::fit_image_attachment(&mut attachment)
                    .map(|note| (attachment, note))
            })
            .await
            .unwrap_or_else(|_| Err("image resize job failed".to_owned())),
            Err(error) => Err(error),
        };
        match loaded {
            Ok((attachment, _))
                if matches!(
                    attachment,
                    crate::extras::multimodal::MediaAttachment::Image { .. }
                ) && let Err(refusal) = crate::extras::multimodal::ensure_vision(
                    &ctx.session.provider,
                    &ctx.session.model,
                ) =>
            {
                write_error(ctx.renderer, format!("not attached: {refusal}"));
                return false;
            }
            Ok((attachment, note)) => {
                let size = attachment.size();
                let mime_str: &str = match &attachment {
                    crate::extras::multimodal::MediaAttachment::Image { mime, .. }
                    | crate::extras::multimodal::MediaAttachment::Audio { mime, .. }
                    | crate::extras::multimodal::MediaAttachment::Document { mime, .. } => {
                        mime.as_str()
                    }
                };
                let mime_str = mime_str.to_string();
                ctx.session.pending_media.push(attachment);
                write_ok(
                    ctx.renderer,
                    format!(
                        "attached: {} ({mime_str}, {size}B){}",
                        path.display(),
                        note.map(|note| format!("; {note}")).unwrap_or_default()
                    ),
                );
            }
            Err(e) => {
                write_error(ctx.renderer, format!("failed to load media: {e}"));
            }
        }
        return false;
    }

    let canonical = path.canonicalize().unwrap_or(path);
    if ctx.context.extra_files.contains(&canonical) {
        write_ok(
            ctx.renderer,
            format!("already added: {}", canonical.display()),
        );
        return false;
    }

    // Count limit: reject before any I/O.
    if ctx.context.extra_files.len() >= MAX_EXTRA_FILES {
        write_error(
            ctx.renderer,
            format!(
                "file limit reached: at most {MAX_EXTRA_FILES} files can be added \
                 (use /drop to remove one first)"
            ),
        );
        return false;
    }

    // Aggregate byte budget: sum already-added file sizes plus this file's metadata size.
    let file_size = std::fs::metadata(&canonical).map(|m| m.len()).unwrap_or(0);
    let current_aggregate: u64 = ctx
        .context
        .extra_files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    if current_aggregate.saturating_add(file_size) > MAX_AGGREGATE_EXTRA_BYTES {
        write_error(
            ctx.renderer,
            format!(
                "aggregate file size would exceed {MAX_AGGREGATE_EXTRA_BYTES} bytes: \
                 use /drop to remove files before adding more"
            ),
        );
        return false;
    }

    // Preload content via spawn_blocking so agent-rebuild paths never block Tokio workers
    // with synchronous filesystem reads.
    let cap = crate::agent::builder::MAX_EXTRA_FILE_BYTES;
    let path_for_task = canonical.clone();
    let content = tokio::task::spawn_blocking(move || {
        crate::agent::builder::read_extra_file_bounded(&path_for_task, cap)
    })
    .await
    .unwrap_or(None);

    let Some(content) = content else {
        write_error(
            ctx.renderer,
            format!("failed to read file: {}", canonical.display()),
        );
        return false;
    };

    ctx.context.extra_files.push(canonical.clone());
    ctx.context
        .extra_file_contents
        .insert(canonical.clone(), Arc::new(content));
    write_ok(
        ctx.renderer,
        format!("added: {} ({file_size}B)", canonical.display()),
    );
    true
}

async fn handle_drop(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        write_usage(
            ctx.renderer,
            "usage: /drop <number|path>...  (numbers: see /add)",
        );
        return Ok(());
    }

    // Numbers are resolved against the list as it is now, before anything is
    // removed, so `/drop 1 2` drops what `/add` showed as 1 and 2.
    let files = ctx.context.extra_files.clone();
    #[cfg(feature = "multimodal")]
    let media: Vec<std::path::PathBuf> = ctx
        .session
        .pending_media
        .iter()
        .map(|m| m.path().to_path_buf())
        .collect();
    #[cfg(not(feature = "multimodal"))]
    let media: Vec<std::path::PathBuf> = Vec::new();
    let mut rebuild = false;
    for arg in &parts[1..] {
        match arg.parse::<usize>() {
            Ok(number) => match drop_target(number, &files, &media) {
                Some(DropTarget::File(path)) => {
                    rebuild |= drop_one(&path.to_string_lossy(), ctx);
                }
                Some(DropTarget::Media(path)) => drop_media(&path, ctx),
                None => write_error(
                    ctx.renderer,
                    format!(
                        "no item {number}: /add lists 1..{} (use ./{number} for a file named {number})",
                        files.len() + media.len()
                    ),
                ),
            },
            Err(_) => rebuild |= drop_one(arg, ctx),
        }
    }
    if rebuild {
        ctx.rebuild_agent().await;
    }
    Ok(())
}

/// The item `/add` numbers `number` (1-based, files then media), as a path.
fn drop_target(
    number: usize,
    files: &[std::path::PathBuf],
    media: &[std::path::PathBuf],
) -> Option<DropTarget> {
    let index = number.checked_sub(1)?;
    if let Some(file) = files.get(index) {
        return Some(DropTarget::File(file.clone()));
    }
    media
        .get(index.checked_sub(files.len())?)
        .map(|path| DropTarget::Media(path.clone()))
}

#[derive(Debug, PartialEq)]
enum DropTarget {
    File(std::path::PathBuf),
    Media(std::path::PathBuf),
}

/// Drop the pending attachment with exactly this path (media paths need not
/// exist on disk, so they are not resolved like `/drop <path>` arguments).
fn drop_media(path: &std::path::Path, ctx: &mut SlashCtx<'_>) {
    #[cfg(feature = "multimodal")]
    if let Some(i) = ctx
        .session
        .pending_media
        .iter()
        .position(|m| m.path() == path)
    {
        ctx.session.pending_media.remove(i);
        write_ok(ctx.renderer, format!("dropped media: {}", path.display()));
        return;
    }
    write_error(
        ctx.renderer,
        format!("not in context: {} (use /add to see)", path.display()),
    );
}

/// Drop one path; returns whether a text file left the context.
fn drop_one(arg: &str, ctx: &mut SlashCtx<'_>) -> bool {
    let path = resolve_path(ctx.workspace.root(), arg);
    let canonical = path.canonicalize().unwrap_or(path);

    // Try extra_files first.
    if let Some(i) = ctx.context.extra_files.iter().position(|f| f == &canonical) {
        ctx.context.extra_files.remove(i);
        ctx.context.extra_file_contents.remove(&canonical);
        write_ok(ctx.renderer, format!("dropped: {}", canonical.display()));
        return true;
    }

    // Try pending_media by path match.
    #[cfg(feature = "multimodal")]
    {
        let canonical_str = canonical.to_string_lossy();
        if let Some(i) = ctx
            .session
            .pending_media
            .iter()
            .position(|m| m.path().to_string_lossy() == canonical_str)
        {
            ctx.session.pending_media.remove(i);
            write_ok(
                ctx.renderer,
                format!("dropped media: {}", canonical.display()),
            );
            return false;
        }
    }

    write_error(
        ctx.renderer,
        format!("not in context: {} (use /add to see)", canonical.display()),
    );
    false
}

async fn handle_drop_all(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let file_count = ctx.context.extra_files.len();
    #[cfg(feature = "multimodal")]
    let media_count = ctx.session.pending_media.len();
    #[cfg(not(feature = "multimodal"))]
    let media_count = 0;

    if file_count == 0 && media_count == 0 {
        write_ok(ctx.renderer, "no files or media to drop");
        return Ok(());
    }

    if file_count > 0 {
        ctx.context.extra_files.clear();
        ctx.context.extra_file_contents.clear();
        ctx.rebuild_agent().await;
    }
    #[cfg(feature = "multimodal")]
    {
        ctx.session.pending_media.clear();
    }

    let mut parts = vec![];
    if file_count > 0 {
        parts.push(format!("{file_count} file(s)"));
    }
    if media_count > 0 {
        parts.push(format!("{media_count} media"));
    }
    write_ok(ctx.renderer, format!("dropped {}", parts.join(", ")));
    Ok(())
}

#[cfg(test)]
mod drop_index_tests {
    use super::{DropTarget, drop_target};
    use std::path::PathBuf;

    #[test]
    fn numbers_address_files_then_media_one_based() {
        let files: Vec<PathBuf> = vec!["/ws/a.txt".into(), "/ws/b.txt".into()];
        let media: Vec<PathBuf> = vec!["/ws/img.png".into()];
        assert_eq!(
            drop_target(1, &files, &media),
            Some(DropTarget::File("/ws/a.txt".into()))
        );
        assert_eq!(
            drop_target(2, &files, &media),
            Some(DropTarget::File("/ws/b.txt".into()))
        );
        assert_eq!(
            drop_target(3, &files, &media),
            Some(DropTarget::Media("/ws/img.png".into()))
        );
        assert_eq!(drop_target(0, &files, &media), None);
        assert_eq!(drop_target(4, &files, &media), None);
        assert_eq!(
            drop_target(1, &[], &media),
            Some(DropTarget::Media("/ws/img.png".into()))
        );
    }
}
