use crate::previewer;
use crate::previewer::vim_help::HelpTagPreview;
use crate::previewer::{get_file_preview, FilePreview};
use crate::stdio_server::job;
use crate::stdio_server::plugin::syntax::{
    convert_raw_ts_highlights_to_vim_highlights, sublime_syntax_by_extension,
    sublime_syntax_highlight, sublime_theme_exists,
};
use crate::stdio_server::provider::{read_dir_entries, Context, ProviderSource};
use crate::stdio_server::vim::{preview_syntax, VimResult};
use crate::tools::ctags::{current_context_tag_async, BufferTag};
use paths::{expand_tilde, truncate_absolute_path};
use pattern::*;
use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind, Result};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;
use sublime_syntax::TokenHighlight;
use utils::display_width;

type SublimeHighlights = Vec<(usize, Vec<TokenHighlight>)>;

/// (start, length, highlight_group)
type LineHighlights = Vec<(usize, usize, String)>;
type TsHighlights = Vec<(usize, LineHighlights)>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VimSyntaxInfo {
    syntax: String,
    fname: String,
}

impl VimSyntaxInfo {
    fn syntax(syntax: String) -> Self {
        Self {
            syntax,
            ..Default::default()
        }
    }

    fn fname(fname: String) -> Self {
        Self {
            fname,
            ..Default::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.syntax.is_empty() && self.fname.is_empty()
    }
}

/// Preview content.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Preview {
    pub lines: Vec<String>,
    /// If no sublime-syntax or tree-sitter highlights,
    /// this field is intended to tell vim what syntax value
    /// should be used for the highlighting. Ideally `syntax`
    /// is returned, otherwise `fname` is returned and
    /// Vim will inspect the syntax value from `fname`.
    #[serde(skip_serializing_if = "VimSyntaxInfo::is_empty")]
    pub vim_syntax_info: VimSyntaxInfo,
    /// Highlights from sublime-syntax highlight engine.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sublime_syntax_highlights: SublimeHighlights,
    /// Highlights from tree-sitter highlight engine.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tree_sitter_highlights: TsHighlights,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hi_lnum: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollbar: Option<(usize, usize)>,
}

impl Preview {
    pub fn new(lines: Vec<String>) -> Self {
        Self {
            lines,
            ..Default::default()
        }
    }

    fn new_file_preview(
        lines: Vec<String>,
        scrollbar: Option<(usize, usize)>,
        vim_syntax_info: VimSyntaxInfo,
    ) -> Self {
        Self {
            lines,
            vim_syntax_info,
            scrollbar,
            ..Default::default()
        }
    }
}

/// Represents various targets for previews in clap provider.
#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub enum PreviewTarget {
    /// List the entries under a directory.
    Directory(PathBuf),
    /// Start from the beginning of a file.
    File(PathBuf),
    /// Represents a specific location in a file identified by its path and line number.
    LineInFile { path: PathBuf, line_number: usize },
    /// Represents a Git commit revision specified by its commit hash.
    GitCommit(String),
    /// Specifically for the `help_tags` provider.
    HelpTags {
        subject: String,
        doc_filename: String,
        runtimepath: String,
    },
}

impl PreviewTarget {
    /// Returns the path associated with the enum variant, or `None` if no path exists.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::File(path) | Self::Directory(path) | Self::LineInFile { path, .. } => Some(path),
            _ => None,
        }
    }
}

fn parse_preview_target(curline: String, ctx: &Context) -> Result<(PreviewTarget, Option<String>)> {
    let err = || {
        Error::new(
            ErrorKind::Other,
            format!(
                "Failed to parse PreviewTarget for provider_id: {} from `{curline}`",
                ctx.provider_id()
            ),
        )
    };

    // Store the line context we see in the search result, but it may be out-dated due to the
    // cache is being used, especially for the providers like grep which potentially have tons of
    // items.
    //
    // If the line we see mismatches the actual line content in the preview, the content in which
    // is always accurate, try to refresh the cache and reload.
    let mut line_content = None;

    let preview_target = match ctx.provider_id() {
        "files" | "git_files" => PreviewTarget::File(ctx.cwd.join(&curline)),
        "recent_files" => PreviewTarget::File(PathBuf::from(&curline)),
        "history" => {
            let path = if curline.starts_with('~') {
                expand_tilde(curline)
            } else {
                ctx.cwd.join(&curline)
            };
            PreviewTarget::File(path)
        }
        "coc_location" | "grep" | "live_grep" | "igrep" => {
            let mut try_extract_file_path = |line: &str| {
                let (fpath, lnum, _col, cache_line) =
                    extract_grep_position(line).ok_or_else(err)?;

                line_content.replace(cache_line.into());

                let fpath = fpath.strip_prefix("./").unwrap_or(fpath);
                let path = ctx.cwd.join(fpath);

                Ok::<_, Error>((path, lnum))
            };

            let (path, line_number) = try_extract_file_path(&curline)?;

            PreviewTarget::LineInFile { path, line_number }
        }
        "dumb_jump" => {
            let (_def_kind, fpath, line_number, _col) =
                extract_jump_line_info(&curline).ok_or_else(err)?;
            let path = ctx.cwd.join(fpath);
            PreviewTarget::LineInFile { path, line_number }
        }
        "blines" => {
            let line_number = extract_blines_lnum(&curline).ok_or_else(err)?;
            let path = ctx.env.start_buffer_path.clone();
            PreviewTarget::LineInFile { path, line_number }
        }
        "tags" => {
            let line_number = extract_buf_tags_lnum(&curline).ok_or_else(err)?;
            let path = ctx.env.start_buffer_path.clone();
            PreviewTarget::LineInFile { path, line_number }
        }
        "proj_tags" => {
            let (line_number, p) = extract_proj_tags(&curline).ok_or_else(err)?;
            let path = ctx.cwd.join(p);
            PreviewTarget::LineInFile { path, line_number }
        }
        "commits" | "bcommits" => {
            let rev = extract_commit_rev(&curline).ok_or_else(err)?;
            PreviewTarget::GitCommit(rev.into())
        }
        unknown_provider_id => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Failed to parse PreviewTarget, you probably forget to \
                    add an implementation for this provider: {unknown_provider_id}",
                ),
            ))
        }
    };

    Ok((preview_target, line_content))
}

/// Returns `true` if the file path of preview file should be truncateted relative to cwd.
fn should_truncate_cwd_relative(provider_id: &str) -> bool {
    const SET: &[&str] = &[
        "files",
        "git_files",
        "grep",
        "live_grep",
        "coc_location",
        "dumb_jump",
        "proj_tags",
    ];
    SET.contains(&provider_id)
}

#[derive(Debug)]
pub struct CachedPreviewImpl<'a> {
    pub ctx: &'a Context,
    pub preview_height: usize,
    pub preview_target: PreviewTarget,
    /// When the line extracted from the cache mismatches the latest
    /// preview line content, which means the cache is outdated, we
    /// should refresh the cache.
    ///
    /// Currently only for the provider `grep`.
    pub cache_line: Option<String>,
}

impl<'a> CachedPreviewImpl<'a> {
    pub fn new(curline: String, preview_height: usize, ctx: &'a Context) -> Result<Self> {
        let (preview_target, cache_line) = parse_preview_target(curline, ctx)?;

        Ok(Self {
            ctx,
            preview_height,
            preview_target,
            cache_line,
        })
    }

    pub fn with_preview_target(
        preview_target: PreviewTarget,
        preview_height: usize,
        ctx: &'a Context,
    ) -> Self {
        Self {
            ctx,
            preview_height,
            preview_target,
            cache_line: None,
        }
    }

    pub async fn get_preview(&self) -> VimResult<(PreviewTarget, Preview)> {
        if let Some(preview) = self
            .ctx
            .preview_manager
            .cached_preview(&self.preview_target)
        {
            return Ok((self.preview_target.clone(), preview));
        }

        let preview = match &self.preview_target {
            PreviewTarget::Directory(path) => self.preview_directory(path)?,
            PreviewTarget::File(path) => self.preview_file(path)?,
            PreviewTarget::LineInFile { path, line_number } => {
                let container_width = self.ctx.preview_winwidth().await?;
                self.preview_file_at(path, *line_number, container_width)
                    .await
            }
            PreviewTarget::GitCommit(rev) => self.preview_commits(rev)?,
            PreviewTarget::HelpTags {
                subject,
                doc_filename,
                runtimepath,
            } => self.preview_help_subject(subject, doc_filename, runtimepath),
        };

        self.ctx
            .preview_manager
            .insert_preview(self.preview_target.clone(), preview.clone());

        Ok((self.preview_target.clone(), preview))
    }

    fn preview_commits(&self, rev: &str) -> Result<Preview> {
        let stdout = self.ctx.exec_cmd(&format!("git show {rev}"))?;
        let stdout_str = String::from_utf8_lossy(&stdout);
        let lines = stdout_str
            .split('\n')
            .take(self.preview_height)
            .map(Into::into)
            .collect::<Vec<_>>();
        let mut preview = Preview::new(lines);
        preview.vim_syntax_info.syntax = "diff".to_string();
        Ok(preview)
    }

    fn preview_help_subject(
        &self,
        subject: &str,
        doc_filename: &str,
        runtimepath: &str,
    ) -> Preview {
        let preview_tag = HelpTagPreview::new(subject, doc_filename, runtimepath);
        if let Some((fname, lines)) = preview_tag.get_help_lines(self.preview_height) {
            let lines = std::iter::once(fname.clone())
                .chain(lines)
                .collect::<Vec<_>>();
            Preview {
                lines,
                hi_lnum: Some(1),
                vim_syntax_info: VimSyntaxInfo::syntax("help".into()),
                ..Default::default()
            }
        } else {
            tracing::debug!(?preview_tag, "Can not find the preview help lines");
            Preview::new(vec!["Can not find the preview help lines".into()])
        }
    }

    fn preview_directory<P: AsRef<Path>>(&self, path: P) -> Result<Preview> {
        let enable_icon = self.ctx.env.icon.enabled();
        let lines = read_dir_entries(&path, enable_icon, Some(self.preview_height))?;
        let mut lines = if lines.is_empty() {
            vec!["<Empty directory>".to_string()]
        } else {
            lines
        };

        let mut title = path.as_ref().display().to_string();
        if title.ends_with(std::path::MAIN_SEPARATOR) {
            title.pop();
        }
        title.push(':');
        lines.insert(0, title);

        Ok(Preview::new(lines))
    }

    fn preview_file<P: AsRef<Path>>(&self, path: P) -> Result<Preview> {
        let path = path.as_ref();

        if !path.is_file() {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Failed to preview as {} is not a file", path.display()),
            ));
        }

        let handle_io_error = |e: &Error| {
            if e.kind() == ErrorKind::NotFound {
                tracing::debug!(
                    "TODO: {} not found, the files cache might be invalid, try refreshing the cache",
                    path.display()
                );
            }
        };

        let (lines, fname) = match (self.ctx.env.is_nvim, self.ctx.env.has_nvim_09) {
            (true, false) => {
                // Title is not available before nvim 0.9
                let max_fname_len = self.ctx.env.display_line_width - 1;
                previewer::preview_file_with_truncated_title(
                    path,
                    self.preview_height,
                    self.max_line_width(),
                    max_fname_len,
                )
                .map_err(|e| {
                    handle_io_error(&e);
                    e
                })?
            }
            _ => {
                let (lines, abs_path) =
                    previewer::preview_file(path, self.preview_height, self.max_line_width())
                        .map_err(|e| {
                            handle_io_error(&e);
                            e
                        })?;
                // cwd is shown via the popup title, no need to include it again.
                let cwd_relative = abs_path.replacen(self.ctx.cwd.as_str(), ".", 1);
                let mut lines = lines;
                lines[0] = cwd_relative;
                (lines, abs_path)
            }
        };

        let total = utils::count_lines(std::fs::File::open(path)?)?;
        let end = lines.len();

        let scrollbar = if self.ctx.env.should_add_scrollbar(end) {
            let preview_winheight = self.ctx.env.display_winheight;

            let length = ((end * preview_winheight) as f32 / total as f32) as usize;

            if length == 0 {
                None
            } else {
                let mut length = preview_winheight.min(length);
                let top_position = if self.ctx.env.preview_border_enabled {
                    length -= if length == preview_winheight { 1 } else { 0 };

                    1usize
                } else {
                    0usize
                };
                Some((top_position, length))
            }
        } else {
            None
        };

        if std::fs::metadata(path)?.len() == 0 {
            let mut lines = lines;
            lines.push("<Empty file>".to_string());
            Ok(Preview::new_file_preview(
                lines,
                scrollbar,
                VimSyntaxInfo::fname(fname),
            ))
        } else if let Some(syntax) = preview_syntax(path) {
            Ok(Preview::new_file_preview(
                lines,
                scrollbar,
                VimSyntaxInfo::syntax(syntax.into()),
            ))
        } else {
            Ok(Preview::new_file_preview(
                lines,
                scrollbar,
                VimSyntaxInfo::fname(fname),
            ))
        }
    }

    async fn preview_file_at(&self, path: &Path, lnum: usize, container_width: usize) -> Preview {
        tracing::debug!(path = ?path.display(), lnum, "Previewing file");

        let fname = path.display().to_string();

        let truncated_preview_header = || {
            let support_float_title = !self.ctx.env.is_nvim || self.ctx.env.has_nvim_09;
            if support_float_title && should_truncate_cwd_relative(self.ctx.provider_id()) {
                // cwd is shown via the popup title, no need to include it again.
                let cwd_relative = fname.replacen(self.ctx.cwd.as_str(), ".", 1);
                format!("{cwd_relative}:{lnum}")
            } else {
                let max_fname_len = container_width - 1 - display_width(lnum);
                let truncated_abs_path = truncate_absolute_path(&fname, max_fname_len);
                format!("{truncated_abs_path}:{lnum}")
            }
        };

        match get_file_preview(path, lnum, self.preview_height) {
            Ok(FilePreview {
                start,
                end,
                total,
                highlight_lnum,
                lines,
            }) => {
                let context_lines = fetch_context_lines(
                    &lines,
                    highlight_lnum,
                    lnum,
                    start,
                    container_width,
                    path,
                    self.ctx.env.is_nvim,
                )
                .await;
                let highlight_lnum = highlight_lnum + context_lines.len();

                let context_lines_is_empty = context_lines.is_empty();

                // 1 (header line) + 1 (1-based line number)
                let line_number_offset = context_lines.len() + 1 + 1;
                let sublime_or_ts_highlights = fetch_syntax_highlights(
                    &lines,
                    path,
                    line_number_offset,
                    self.max_line_width(),
                    start..end + 1,
                    context_lines.len(),
                );

                let header_line = truncated_preview_header();
                let lines = std::iter::once(header_line)
                    .chain(context_lines.into_iter())
                    .chain(self.truncate_preview_lines(lines.into_iter()))
                    .collect::<Vec<_>>();

                let scrollbar = if self.ctx.env.should_add_scrollbar(total) {
                    let start = if context_lines_is_empty {
                        start.saturating_sub(3)
                    } else {
                        start
                    };
                    let preview_winheight = self.ctx.env.display_winheight;
                    let length =
                        (((end - start) * preview_winheight) as f32 / total as f32) as usize;
                    let top_position = (start * preview_winheight) as f32 / total as f32;

                    if length == 0 {
                        None
                    } else {
                        let mut length = preview_winheight.min(length);
                        let top_position = if self.ctx.env.preview_border_enabled {
                            length -= if length == preview_winheight { 1 } else { 0 };

                            1usize.max(top_position as usize)
                        } else {
                            top_position as usize
                        };

                        Some((top_position, length))
                    }
                } else {
                    None
                };

                let mut preview = Preview {
                    lines,
                    hi_lnum: Some(highlight_lnum),
                    scrollbar,
                    ..Default::default()
                };

                match sublime_or_ts_highlights {
                    SublimeOrTreeSitter::Sublime(v) => {
                        preview.sublime_syntax_highlights = v;
                    }
                    SublimeOrTreeSitter::TreeSitter(v) => {
                        preview.tree_sitter_highlights = v;
                    }
                    SublimeOrTreeSitter::Neither => {
                        if let Some(syntax) = preview_syntax(path) {
                            preview.vim_syntax_info.syntax = syntax.into();
                        } else {
                            preview.vim_syntax_info.fname = fname;
                        }
                    }
                }

                preview
            }
            Err(err) => {
                tracing::error!(
                    ?path,
                    provider_id = %self.ctx.provider_id(),
                    "Couldn't read first lines: {err:?}",
                );
                let header_line = truncated_preview_header();
                let lines = vec![
                    header_line,
                    format!("Error while previewing {}: {err}", path.display()),
                ];
                Preview {
                    lines,
                    vim_syntax_info: VimSyntaxInfo::fname(fname),
                    ..Default::default()
                }
            }
        }
    }

    // TODO: Only run for these provider using custom shell command.
    #[allow(unused)]
    fn try_refresh_cache(&self, latest_line: &str) {
        if self.ctx.provider_id() == "grep" {
            if let Some(ref cache_line) = self.cache_line {
                if cache_line != latest_line {
                    tracing::debug!(?latest_line, ?cache_line, "The cache is probably outdated");

                    let shell_cmd = crate::tools::rg::rg_shell_command(&self.ctx.cwd);
                    let job_id = utils::calculate_hash(&shell_cmd);

                    if job::reserve(job_id) {
                        let ctx = self.ctx.clone();

                        // TODO: Refresh with a timeout.
                        tokio::task::spawn_blocking(move || {
                            tracing::debug!(cwd = ?ctx.cwd, "Refreshing grep cache");
                            let new_digest = match crate::tools::rg::refresh_cache(&ctx.cwd) {
                                Ok(digest) => {
                                    tracing::debug!(total = digest.total, "Refreshed grep cache");
                                    digest
                                }
                                Err(e) => {
                                    tracing::error!(error = ?e, "Failed to refresh grep cache");
                                    return;
                                }
                            };
                            let new = ProviderSource::CachedFile {
                                total: new_digest.total,
                                path: new_digest.cached_path,
                                refreshed: true,
                            };
                            ctx.set_provider_source(new);
                            job::unreserve(job_id);

                            if !ctx.terminated.load(Ordering::SeqCst) {
                                let _ = ctx.vim.echo_info("Out-dated cache refreshed");
                            }
                        });
                    } else {
                        tracing::debug!(
                            cwd = ?self.ctx.cwd,
                            "Another grep job is running, skip freshing the cache"
                        );
                    }
                }
            }
        }
    }

    /// Truncates the lines that are awfully long as vim might have some performance issue with
    /// them.
    ///
    /// Ref https://github.com/liuchengxu/vim-clap/issues/543
    fn truncate_preview_lines(
        &self,
        lines: impl Iterator<Item = String>,
    ) -> impl Iterator<Item = String> {
        previewer::truncate_lines(lines, self.max_line_width())
    }

    /// Returns the maximum line width.
    #[inline]
    fn max_line_width(&self) -> usize {
        2 * self.ctx.env.display_line_width
    }
}

async fn context_tag_with_timeout(path: &Path, lnum: usize) -> Option<BufferTag> {
    const TIMEOUT: Duration = Duration::from_millis(300);

    match tokio::time::timeout(TIMEOUT, current_context_tag_async(path, lnum)).await {
        Ok(res) => res,
        Err(_) => {
            tracing::debug!(timeout = ?TIMEOUT, ?path, lnum, "⏳ Did not get the context tag in time");
            None
        }
    }
}

async fn fetch_context_lines(
    lines: &[String],
    highlight_lnum: usize,
    lnum: usize,
    start: usize,
    container_width: usize,
    path: &Path,
    is_nvim: bool,
) -> Vec<String> {
    // Some checks against the latest preview line.
    let Some(line) = lines.get(highlight_lnum - 1) else {
        return Vec::new();
    };

    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return Vec::new();
    };

    let skip_context_tag = {
        const BLACK_LIST: &[&str] = &["log", "txt", "lock", "toml", "yaml", "mod", "conf"];

        BLACK_LIST.contains(&ext) || dumb_analyzer::is_comment(line, ext)
    };

    if skip_context_tag {
        return Vec::new();
    };

    let generate_border_line = |container_width: usize, is_nvim: bool| -> String {
        // Vim has a different border width.
        "─".repeat(if is_nvim {
            container_width
        } else {
            container_width - 2
        })
    };

    let mut context_lines = Vec::new();

    match context_tag_with_timeout(path, lnum).await {
        Some(tag) if tag.line_number < start => {
            context_lines.reserve_exact(3);

            let border_line = generate_border_line(container_width, is_nvim);

            context_lines.push(border_line.clone());

            // Truncate the right of pattern, 2 whitespaces + 💡
            let max_pattern_len = container_width - 4;
            let pattern = tag.trimmed_pattern();
            let (mut context_line, to_push) = if pattern.len() > max_pattern_len {
                // Use the chars instead of indexing the str to avoid the char boundary error.
                let p: String = pattern.chars().take(max_pattern_len - 4 - 2).collect();
                (p, "..  💡")
            } else {
                (String::from(pattern), "  💡")
            };
            context_line.reserve(to_push.len());
            context_line.push_str(to_push);
            context_lines.push(context_line);

            context_lines.push(border_line);
        }
        _ => {
            // No context lines if no tag found prior to the line number.
        }
    }

    context_lines
}

enum SublimeOrTreeSitter {
    Sublime(SublimeHighlights),
    TreeSitter(TsHighlights),
    Neither,
}

// TODO: this might be slow for larger files (over 100k lines) as tree-sitter will have to
// parse the whole file to obtain the highlight info. We may make the highlighting async.
fn fetch_syntax_highlights(
    lines: &[String],
    path: &Path,
    line_number_offset: usize,
    max_line_width: usize,
    range: Range<usize>,
    context_lines_offset: usize,
) -> SublimeOrTreeSitter {
    use crate::config::HighlightEngine;

    let provider_config = &crate::config::config().provider;

    match provider_config.preview_highlight_engine {
        HighlightEngine::SublimeSyntax => {
            const THEME: &str = "Visual Studio Dark+";

            let theme = match &provider_config.sublime_syntax_color_scheme {
                Some(theme) => {
                    if sublime_theme_exists(theme) {
                        theme.as_str()
                    } else {
                        tracing::warn!(
                            "preview color theme {theme} not found, fallback to {THEME}"
                        );
                        THEME
                    }
                }
                None => THEME,
            };

            path.extension()
                .and_then(|s| s.to_str())
                .and_then(sublime_syntax_by_extension)
                .map(|syntax| {
                    //  Same reason as [`Self::truncate_preview_lines()`], if a line is too
                    //  long and the query is short, the highlights can be enomerous and
                    //  cause the Vim frozen due to the too many highlight works.
                    let max_len = max_line_width;
                    let lines = lines.iter().map(|s| {
                        let len = s.len().min(max_len);
                        &s[..len]
                    });
                    sublime_syntax_highlight(syntax, lines, line_number_offset, theme)
                })
                .map(SublimeOrTreeSitter::Sublime)
                .unwrap_or(SublimeOrTreeSitter::Neither)
        }
        HighlightEngine::TreeSitter => {
            // TODO: max file size limit and max line limit?
            path.extension()
                .and_then(|s| s.to_str())
                .and_then(tree_sitter::Language::try_from_extension)
                .and_then(|language| {
                    let Ok(source_code) = std::fs::read(path) else {
                        return None;
                    };

                    let Ok(raw_highlights) = tree_sitter::highlight(language, &source_code) else {
                        return None;
                    };

                    let line_start = range.start;
                    let ts_highlights = convert_raw_ts_highlights_to_vim_highlights(
                        &raw_highlights,
                        language,
                        Some(range),
                    );

                    Some(
                        ts_highlights
                            .into_iter()
                            .map(|(line_number, line_highlights)| {
                                let line_number_in_preview_win =
                                    line_number - line_start + 1 + context_lines_offset;

                                // Workaround the lifetime issue, nice to remove this allocation
                                // `group.to_string()` as it's essentially `&'static str`.
                                let line_highlights = line_highlights
                                    .into_iter()
                                    .filter_map(|(start, length, group)| {
                                        if start + length > max_line_width {
                                            None
                                        } else {
                                            Some((start, length, group.to_string()))
                                        }
                                    })
                                    .collect();

                                (line_number_in_preview_win, line_highlights)
                            })
                            .collect(),
                    )
                })
                .map(SublimeOrTreeSitter::TreeSitter)
                .unwrap_or(SublimeOrTreeSitter::Neither)
        }
        HighlightEngine::Vim => SublimeOrTreeSitter::Neither,
    }
}
