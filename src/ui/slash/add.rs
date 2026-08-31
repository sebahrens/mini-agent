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
            for f in &ctx.context.extra_files {
                let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                write_result(ctx.renderer, format!("  {} ({size}B)", f.display()));
            }
            #[cfg(feature = "multimodal")]
            for m in &ctx.session.pending_media {
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
                    format!("  [{kind}] {} ({size}B)", m.path().display()),
                );
            }
        }
        return Ok(());
    }

    let path = resolve_path(ctx.workspace.root(), parts[1]);

    if !path.exists() {
        write_error(ctx.renderer, format!("file not found: {}", path.display()));
        return Ok(());
    }
    if !path.is_file() {
        write_error(ctx.renderer, format!("not a file: {}", path.display()));
        return Ok(());
    }

    #[cfg(feature = "multimodal")]
    if crate::extras::multimodal::detect_media(&path).is_some() {
        match crate::extras::multimodal::load_attachment(&path) {
            Ok(attachment) => {
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
                    format!("attached: {} ({mime_str}, {size}B)", path.display()),
                );
            }
            Err(e) => {
                write_error(ctx.renderer, format!("failed to load media: {e}"));
            }
        }
        return Ok(());
    }

    let canonical = path.canonicalize().unwrap_or(path);
    if ctx.context.extra_files.contains(&canonical) {
        write_ok(
            ctx.renderer,
            format!("already added: {}", canonical.display()),
        );
        return Ok(());
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
        return Ok(());
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
        return Ok(());
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
        return Ok(());
    };

    ctx.context.extra_files.push(canonical.clone());
    ctx.context
        .extra_file_contents
        .insert(canonical.clone(), Arc::new(content));
    ctx.rebuild_agent().await;
    write_ok(
        ctx.renderer,
        format!("added: {} ({file_size}B)", canonical.display()),
    );
    Ok(())
}

async fn handle_drop(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        write_error(ctx.renderer, "usage: /drop <path-or-index>");
        return Ok(());
    }

    let path = resolve_path(ctx.workspace.root(), parts[1]);
    let canonical = path.canonicalize().unwrap_or(path);

    // Try extra_files first.
    if let Some(i) = ctx.context.extra_files.iter().position(|f| f == &canonical) {
        ctx.context.extra_files.remove(i);
        ctx.context.extra_file_contents.remove(&canonical);
        ctx.rebuild_agent().await;
        write_ok(ctx.renderer, format!("dropped: {}", canonical.display()));
        return Ok(());
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
            return Ok(());
        }
        // Also try parsing as an index into the pending_media list.
        if let Ok(idx) = parts[1].parse::<usize>()
            && idx < ctx.session.pending_media.len()
        {
            let removed = ctx.session.pending_media.remove(idx);
            write_ok(
                ctx.renderer,
                format!("dropped media: {}", removed.path().display()),
            );
            return Ok(());
        }
    }

    write_error(
        ctx.renderer,
        format!("not in context: {} (use /add to see)", canonical.display()),
    );
    Ok(())
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
