use std::path::Path;
use std::sync::Arc;

use super::{CommandDescriptor, Provider, SearchResult, SCORE_CONTENT};
use crate::content_index::ContentIndex;

pub struct ContentProvider {
    index: Arc<ContentIndex>,
    /// SQL row cap for the FTS query. Mirrors the launcher's `max_results` so we
    /// only `snippet()` the rows that will actually be shown - snippet generation
    /// is the dominant per-query cost, so fetching the old fixed 50 and discarding
    /// all but `max_results` wasted most of it on common-word queries.
    max_results: usize,
}

impl ContentProvider {
    pub fn new(index: Arc<ContentIndex>, max_results: usize) -> Self {
        Self { index, max_results }
    }
}

impl Provider for ContentProvider {
    fn id(&self) -> &str {
        "content"
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        use crate::providers::command::{CommandRoute, CommandSource, ModeKind};
        vec![CommandDescriptor {
            id: "cmd:contents".to_string(),
            title: "Search File Contents".to_string(),
            chip: "Contents".to_string(),
            subtitle: Some("Full-text search inside files".to_string()),
            source: CommandSource::Builtin,
            mode_kind: ModeKind::Scope,
            keywords: vec![
                "contents".into(),
                "grep".into(),
                "search".into(),
                "text".into(),
                "fulltext".into(),
            ],
            placeholder: Some("Search file contents…".to_string()),
            min_query_len: 2,
            result_kind: "file".to_string(),
            glyph: Some("search".to_string()),
            icon_data_uri: None,
            default_shortcut: None,
            opens_form: false,
            uncapped: false,
            volatile: false,
            route: CommandRoute::Builtin { provider_id: "content".to_string() },
        }]
    }

    // search_scoped falls through to search(): the whole query is the term.
    fn search(&self, query: &str) -> Vec<SearchResult> {
        // Content scope is selected by the caller (PluginRegistry::search_scope),
        // so the raw query is the search term - no activation prefix to strip.
        let q = query.trim();
        if q.len() < 2 {
            return vec![];
        }

        // Parse into deduped, stopword-stripped FTS terms. Dedup + stopword
        // stripping bound the common-term cost; an all-stopword query comes back
        // `ranked = false` so the search skips the corpus-wide bm25 sort.
        let Some(parsed) = crate::content_index::parse_content_query(q) else {
            return vec![];
        };
        // FTS5 treats space-separated terms as AND; the trailing (being-typed)
        // term is prefix-matched so partial words surface results incrementally.
        let fts_query = parsed.fts_match();

        match self.index.search(&fts_query, self.max_results.max(1), parsed.ranked) {
            Ok(results) => results
                .into_iter()
                .map(|(path, rank, snip, mtime, size)| {
                    let p = Path::new(&path);
                    let title = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&path)
                        .to_owned();
                    let parent = p
                        .parent()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .to_owned();
                    let escaped = path.replace('"', "\\\"");
                    // `match_page` is computed lazily by the `content_match_section`
                    // command only for the file actually being previewed - computing
                    // it here ran a full per-PDF page rescan for every one of the (up
                    // to 50) results on each keystroke, which for common-word queries
                    // dominated content-search latency.
                    let created = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.created().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    SearchResult {
                        id: format!("file:{path}"),
                        title,
                        subtitle: Some(parent),
                        snippet: Some(snip),
                        kind: "file".to_string(),
                        score: SCORE_CONTENT + (-rank as f32) * 1000.0,
                        exec: Some(format!("xdg-open \"{escaped}\"")),
                        file_size: if size > 0 { Some(size) } else { None },
                        created,
                        modified: if mtime > 0 { Some(mtime as u64) } else { None },
                        ..Default::default()
                    }
                })
                .collect(),
            Err(e) => {
                eprintln!("[content] search error: {e}");
                vec![]
            }
        }
    }
}
