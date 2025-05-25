//! DevDocs Library
//!
//! A library for managing DevDocs documentation locally with fuzzy search capabilities.

use bitcode;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use nucleo::{Config, Matcher, Nucleo, Utf32Str};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const DEVDOCS_BASE_URL: &str = "https://devdocs.io";
const DOCUMENTS_BASE_URL: &str = "https://documents.devdocs.io";
const CACHE_DURATION_DAYS: u64 = 7;

#[derive(Debug, thiserror::Error)]
pub enum DevDocsError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Documentation '{0}' not found")]
    DocNotFound(String),
    #[error("Documentation '{0}' already exists")]
    DocAlreadyExists(String),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("Invalid slug: {0}")]
    InvalidSlug(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Doc {
    pub name: String,
    pub slug: String,
    #[serde(rename = "type")]
    pub doc_type: String,
    pub links: Option<Links>,
    pub mtime: u64,
    pub db_size: usize,
    pub attribution: Option<String>,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Links {
    pub home: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocIndex {
    pub entries: Vec<Entry>,
    pub types: Vec<EntryType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryType {
    pub name: String,
    pub count: usize,
    pub slug: String,
}

#[derive(Debug, Clone)]
pub struct SearchableEntry {
    pub entry: Entry,
    pub doc_slug: String,
    pub doc_name: String,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub entry: SearchableEntry,
    pub score: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedDoc {
    doc: Doc,
    index: DocIndex,
    cached_at: u64,
}

#[derive(Debug)]
pub struct DevDocsManager {
    client: Client,
    data_dir: PathBuf,
    cache: RwLock<HashMap<String, CachedDoc>>,
    available_docs: RwLock<Option<(Vec<Doc>, u64)>>,
}

impl DevDocsManager {
    /// Create a new DevDocs manager
    pub fn new() -> Result<Self> {
        let data_dir = dirs::data_local_dir()
            .context("Failed to get local data directory")?
            .join("devdocs");

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("devdocs-rs/1.0")
            .build()?;

        Ok(Self {
            client,
            data_dir,
            cache: RwLock::new(HashMap::new()),
            available_docs: RwLock::new(None),
        })
    }

    /// Initialize the manager (create directories, load cache)
    pub async fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir).await?;
        self.load_cache().await?;
        Ok(())
    }

    /// Refresh the list of available documentation
    pub async fn refresh_available_docs(&self) -> Result<Vec<Doc>> {
        info!("Refreshing available documentation list");

        let url = format!("{}/docs.json", DEVDOCS_BASE_URL);
        let response = self.client.get(&url).send().await?;
        let docs: Vec<Doc> = response.json().await?;

        let now = current_timestamp();
        let mut available = self.available_docs.write().await;
        *available = Some((docs.clone(), now));

        self.save_available_docs(&docs).await?;

        info!("Refreshed {} available documentation entries", docs.len());
        Ok(docs)
    }

    /// Get the list of available documentation (cached or fresh)
    pub async fn get_available_docs(&self) -> Result<Vec<Doc>> {
        let available = self.available_docs.read().await;

        if let Some((docs, cached_at)) = &*available {
            if current_timestamp() - cached_at < CACHE_DURATION_DAYS * 24 * 60 * 60 {
                return Ok(docs.clone());
            }
        }

        drop(available);
        self.refresh_available_docs().await
    }

    async fn split_into_html(&self, slug: &str) -> Result<()> {
        let total_content = self.download_doc_content(slug).await?;

        total_content.into_iter().for_each(|(name, contents)| {
            let key = self.data_dir.join(name);
            let parent_dir = key.parent().unwrap();
            std::fs::create_dir_all(parent_dir).unwrap();
            std::fs::write(add_html_ext(key), ensure_html_extensions(&contents)).unwrap();
        });

        Ok(())
    }

    /// Add a new documentation
    pub async fn add_doc(&self, slug: &str) -> Result<()> {
        if self.is_doc_installed(slug).await? {
            warn!("Doc is already installed, skipping.");
            return Ok(());
        }

        let available_docs = self.get_available_docs().await?;
        let doc = available_docs
            .iter()
            .find(|d| d.slug == slug)
            .ok_or_else(|| DevDocsError::DocNotFound(slug.to_string()))?
            .clone();

        info!("Adding documentation: {} ({})", doc.name, doc.slug);

        // Download index and content concurrently
        let index = self.download_doc_index(&doc.slug).await?;

        self.split_into_html(&doc.slug).await?;

        let cached_doc = CachedDoc {
            doc,
            index,
            cached_at: current_timestamp(),
        };

        // Update cache and save to disk
        let mut cache = self.cache.write().await;
        cache.insert(slug.to_string(), cached_doc.clone());
        drop(cache);

        self.save_doc_cache(slug, &cached_doc).await?;

        info!("Successfully added documentation: {}", slug);
        Ok(())
    }

    /// Remove a documentation
    pub async fn remove_doc(&self, slug: &str) -> Result<()> {
        if !self.is_doc_installed(slug).await? {
            return Err(DevDocsError::DocNotFound(slug.to_string()).into());
        }

        info!("Removing documentation: {}", slug);

        // Remove from cache
        let mut cache = self.cache.write().await;
        cache.remove(slug);
        drop(cache);

        // Remove from disk
        let doc_path = self.data_dir.join(format!("{}.json", slug));
        if doc_path.exists() {
            fs::remove_file(doc_path).await?;
        }

        info!("Successfully removed documentation: {}", slug);
        Ok(())
    }

    /// Download all available documentation
    pub async fn download_all(&self) -> Result<()> {
        let available_docs = self.get_available_docs().await?;
        let installed_docs = self.list_installed_docs().await?;

        let to_download: Vec<_> = available_docs
            .into_iter()
            .filter(|doc| !installed_docs.contains(&doc.slug))
            .collect();

        info!("Downloading {} documentation entries", to_download.len());

        // Download in batches to avoid overwhelming the server
        const BATCH_SIZE: usize = 5;
        for batch in to_download.chunks(BATCH_SIZE) {
            let futures = batch.iter().map(|doc| self.add_doc(&doc.slug));
            let results: Vec<_> = futures::future::join_all(futures).await;

            for (doc, result) in batch.iter().zip(results) {
                if let Err(e) = result {
                    warn!("Failed to download {}: {}", doc.slug, e);
                }
            }
        }

        Ok(())
    }

    /// List installed documentation
    pub async fn list_installed_docs(&self) -> Result<Vec<String>> {
        let cache = self.cache.read().await;
        Ok(cache.keys().cloned().collect())
    }

    /// Check if a documentation is installed
    pub async fn is_doc_installed(&self, slug: &str) -> Result<bool> {
        let cache = self.cache.read().await;
        Ok(cache.contains_key(slug))
    }

    /// Get information about an installed documentation
    pub async fn get_doc_info(&self, slug: &str) -> Result<Doc> {
        let cache = self.cache.read().await;
        let cached_doc = cache
            .get(slug)
            .ok_or_else(|| DevDocsError::DocNotFound(slug.to_string()))?;
        Ok(cached_doc.doc.clone())
    }

    /// Search through installed documentation with fuzzy matching
    pub async fn search(&self, query: &str, limit: Option<usize>) -> Result<Vec<SearchResult>> {
        let cache = self.cache.read().await;
        let limit = limit.unwrap_or(50);

        if cache.is_empty() {
            return Ok(vec![]);
        }

        // Collect all searchable entries
        let mut entries = Vec::new();
        for (slug, cached_doc) in cache.iter() {
            for entry in &cached_doc.index.entries {
                entries.push(SearchableEntry {
                    entry: entry.clone(),
                    doc_slug: slug.clone(),
                    doc_name: cached_doc.doc.name.clone(),
                });
            }
        }

        drop(cache);

        use std::cell::RefCell;
        use thread_local::ThreadLocal;
        // Perform fuzzy search
        let matcher = Matcher::new(Config::DEFAULT);
        let tls: ThreadLocal<RefCell<Matcher>> = ThreadLocal::new();

        let mut pattern_buf: Vec<char> = Vec::new();

        let pattern = Utf32Str::new(query, &mut pattern_buf);

        use rayon::prelude::*;
        // Pattern match
        let mut results: Vec<SearchResult> = entries
            .into_par_iter()
            .map(|entry| {
                // each thread/thread-pool task gets its own buffer
                let mut entry_buf = Vec::new();

                let cell = tls.get_or(|| RefCell::new(matcher.clone()));
                let mut matcher = cell.borrow_mut();

                let text = format!("{} {}", entry.entry.name, entry.entry.entry_type);
                let full = Utf32Str::new(text.as_str(), &mut entry_buf);

                let score = matcher.fuzzy_match(full, pattern).unwrap_or(0);

                SearchResult { entry, score }
            })
            .collect();

        // Sort by score (higher is better)
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results.into_iter().take(limit).collect())
    }

    // /// Get the content of a specific documentation page
    // pub async fn get_page_content(&self, slug: &str, path: &str) -> Result<String> {
    //     let cache = self.cache.read().await;
    //     let cached_doc = cache
    //         .get(slug)
    //         .ok_or_else(|| DevDocsError::DocNotFound(slug.to_string()))?;

    //     cached_doc
    //         .content
    //         .get(path)
    //         .cloned()
    //         .ok_or_else(|| DevDocsError::DocNotFound(format!("{}#{}", slug, path)).into())
    // }

    /// Update a specific documentation
    pub async fn update_doc(&self, slug: &str) -> Result<()> {
        if !self.is_doc_installed(slug).await? {
            return Err(DevDocsError::DocNotFound(slug.to_string()).into());
        }

        // Remove and re-add
        self.remove_doc(slug).await?;
        self.add_doc(slug).await?;

        Ok(())
    }

    /// Update all installed documentation
    pub async fn update_all(&self) -> Result<()> {
        let installed_docs = self.list_installed_docs().await?;

        info!("Updating {} documentation entries", installed_docs.len());

        for slug in installed_docs {
            if let Err(e) = self.update_doc(&slug).await {
                warn!("Failed to update {}: {}", slug, e);
            }
        }

        Ok(())
    }

    // Private helper methods

    async fn download_doc_index(&self, slug: &str) -> Result<DocIndex> {
        let url = format!("{}/{}/index.json", DOCUMENTS_BASE_URL, slug);
        debug!("Downloading index: {}", url);

        let response = self.client.get(&url).send().await?;
        let index: DocIndex = response.json().await?;

        Ok(index)
    }

    async fn download_doc_content(&self, slug: &str) -> Result<HashMap<String, String>> {
        let url = format!("{}/{}/db.json", DOCUMENTS_BASE_URL, slug);
        debug!("Downloading content: {}", url);

        let response = self.client.get(&url).send().await?;
        let content: HashMap<String, String> = response.json().await?;

        Ok(content)
    }

    async fn save_doc_cache(&self, slug: &str, cached_doc: &CachedDoc) -> Result<()> {
        use bitcode;
        let path = self.data_dir.join(format!("{}.bin", slug));
        let data = bitcode::serialize(&cached_doc.index)?;
        fs::write(path, data).await?;
        Ok(())
    }

    async fn load_cache(&self) -> Result<()> {
        let mut entries = fs::read_dir(&self.data_dir).await?;
        let mut cache = self.cache.write().await;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if stem == "available_docs" {
                        continue; // Skip available docs cache
                    }

                    match fs::read(&path).await {
                        Ok(content) => match bitcode::deserialize::<CachedDoc>(&content) {
                            Ok(cached_doc) => {
                                cache.insert(stem.to_string(), cached_doc);
                            }
                            Err(e) => {
                                warn!("Failed to parse cached doc {}: {}", stem, e);
                            }
                        },
                        Err(e) => {
                            warn!("Failed to read cached doc {}: {}", stem, e);
                        }
                    }
                }
            }
        }

        // Load available docs cache
        if let Ok(content) = fs::read_to_string(self.data_dir.join("available_docs.json")).await {
            if let Ok((docs, cached_at)) = serde_json::from_str::<(Vec<Doc>, u64)>(&content) {
                *self.available_docs.write().await = Some((docs, cached_at));
            }
        }

        info!("Loaded {} cached documentation entries", cache.len());
        Ok(())
    }

    async fn save_available_docs(&self, docs: &[Doc]) -> Result<()> {
        let path = self.data_dir.join("available_docs.json");
        let data = (docs, current_timestamp());
        let json = serde_json::to_string_pretty(&data)?;
        fs::write(path, json).await?;
        Ok(())
    }
}

impl Default for DevDocsManager {
    fn default() -> Self {
        Self::new().expect("Failed to create DevDocsManager")
    }
}

// Helper functions

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
}

fn add_html_ext(mut path: PathBuf) -> PathBuf {
    if let Some(ext) = path.extension() {
        // If we find an extension, like in the sub-trait thing, extend it with html
        let mut new_ext = ext.to_os_string();
        new_ext.push(".html");
        path.set_extension(new_ext);
    } else {
        // no extension, just html
        path.set_extension("html");
    }
    path
}

use regex::{Captures, Regex};
fn ensure_html_extensions(html: &str) -> String {
    // match href="..."; group 1 is the URL
    let re = Regex::new(r#"href="([^"]+)""#).unwrap();

    re.replace_all(html, |caps: &Captures| {
        let url = &caps[1];

        // leave absolute URLs alone
        if url.starts_with("http://") || url.starts_with("https://") {
            format!(r#"href="{}""#, url)
        } else {
            // split off a fragment if any
            let (path, fragment) = match url.split_once('#') {
                Some((p, f)) => (p, format!("#{}", f)),
                None => (url, String::new()),
            };
            // only add `.html` if it's not already there
            let path = if path.ends_with(".html") {
                path.to_string()
            } else {
                format!("{}.html", path)
            };
            // reassemble
            format!(r#"href="{}{}""#, path, fragment)
        }
    })
    .into_owned()
}

// Re-exports for convenience
pub use nucleo;
pub use reqwest;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio;

    #[tokio::test]
    async fn test_manager_creation() {
        let manager = DevDocsManager::new().unwrap();
        assert!(manager.data_dir.to_string_lossy().contains("devdocs"));
    }

    #[tokio::test]
    async fn test_search() {
        env_logger::init();
        let manager = DevDocsManager::new().unwrap();

        manager.add_doc("rust").await.unwrap();
        let result = manager.search("yeet", None).await.unwrap();

        println!("{result:?}");
        assert!(manager.data_dir.to_string_lossy().contains("devdocs"));
    }

    #[tokio::test]
    async fn test_get_available_docs() {
        let manager = DevDocsManager::new().unwrap();
        manager.init().await.unwrap();

        // This test requires network access
        if std::env::var("SKIP_NETWORK_TESTS").is_err() {
            let docs = manager.get_available_docs().await.unwrap();
            assert!(!docs.is_empty());
        }
    }
}
