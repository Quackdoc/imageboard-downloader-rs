//! Post extractor for `https://danbooru.donmai.us`
//!
//! The danbooru extractor has the following features:
//! - Authentication
//! - Native blacklist (defined in user profile page)
//!
use super::{Auth, Extractor};
use crate::{blacklist::BlacklistFilter, error::ExtractorError};
use async_trait::async_trait;
use ibdl_common::serde_json::Value;
use ibdl_common::{
    ahash::AHashSet,
    auth::{auth_prompt, ImageboardConfig},
    client, join_tags,
    log::debug,
    post::{rating::Rating, Post, PostQueue},
    reqwest::Client,
    tokio::time::Instant,
    ImageBoards,
};
use std::fmt::Display;

/// Main object to download posts
#[derive(Debug)]
pub struct DanbooruExtractor {
    client: Client,
    tags: Vec<String>,
    tag_string: String,
    auth_state: bool,
    auth: ImageboardConfig,
    safe_mode: bool,
    disable_blacklist: bool,
    total_removed: u64,
}

#[async_trait]
impl Extractor for DanbooruExtractor {
    fn new<S>(tags: &[S], safe_mode: bool, disable_blacklist: bool) -> Self
    where
        S: ToString + Display,
    {
        // Use common client for all connections with a set User-Agent
        let client = client!(ImageBoards::Danbooru);

        // Set Safe mode status
        let safe_mode = safe_mode;

        let strvec: Vec<String> = tags
            .iter()
            .map(|t| {
                let st: String = t.to_string();
                st
            })
            .collect();

        // Merge all tags in the URL format
        let tag_string = join_tags!(strvec);
        debug!("Tag List: {}", tag_string);

        Self {
            client,
            tags: strvec,
            tag_string,
            auth_state: false,
            auth: ImageboardConfig::default(),
            safe_mode,
            disable_blacklist,
            total_removed: 0,
        }
    }

    async fn search(&mut self, page: usize) -> Result<PostQueue, ExtractorError> {
        Self::validate_tags(self).await?;

        let mut posts = Self::get_post_list(self, page).await?;

        posts.sort();
        posts.reverse();

        let qw = PostQueue {
            posts,
            tags: self.tags.clone(),
        };

        Ok(qw)
    }

    async fn full_search(
        &mut self,
        start_page: Option<usize>,
        limit: Option<usize>,
    ) -> Result<PostQueue, ExtractorError> {
        Self::validate_tags(self).await?;

        let blacklist = BlacklistFilter::init(
            ImageBoards::Danbooru,
            &self.auth.user_data.blacklisted_tags,
            self.safe_mode,
            self.disable_blacklist,
        )
        .await?;

        let mut fvec = if let Some(size) = limit {
            Vec::with_capacity(size)
        } else {
            Vec::new()
        };

        let mut page = 1;

        loop {
            let position = if let Some(n) = start_page {
                page + n
            } else {
                page
            };

            debug!("Scanning page {}", position);

            let posts = Self::get_post_list(self, position).await?;
            let size = posts.len();

            if size == 0 {
                println!();
                break;
            }

            let list = if !self.disable_blacklist || self.safe_mode {
                let (removed, posts) = blacklist.filter(posts);
                self.total_removed += removed;
                posts
            } else {
                posts
            };

            fvec.extend(list);

            if let Some(num) = limit {
                if fvec.len() >= num {
                    break;
                }
            }

            if page == 100 {
                break;
            }

            page += 1;
        }

        fvec.sort();
        fvec.reverse();

        let fin = PostQueue {
            posts: fvec,
            tags: self.tags.clone(),
        };
        Ok(fin)
    }

    fn client(self) -> Client {
        self.client
    }

    fn total_removed(&self) -> u64 {
        self.total_removed
    }
}

#[async_trait]
impl Auth for DanbooruExtractor {
    async fn auth(&mut self, prompt: bool) -> Result<(), ExtractorError> {
        auth_prompt(prompt, ImageBoards::Danbooru, &self.client).await?;

        if let Some(creds) = ImageBoards::Danbooru.read_config_from_fs().await? {
            self.auth = creds;
            self.auth_state = true;
            return Ok(());
        }

        self.auth_state = false;
        Ok(())
    }
}

impl DanbooruExtractor {
    async fn validate_tags(&self) -> Result<(), ExtractorError> {
        if self.tags.len() > 2 {
            return Err(ExtractorError::TooManyTags {
                current: self.tags.len(),
                max: 2,
            });
        };

        let count_endpoint = format!(
            "{}?tags={}",
            ImageBoards::Danbooru.post_count_url().unwrap(),
            &self.tag_string
        );

        // Get an estimate of total posts and pages to search
        let request = if self.auth_state {
            debug!("[AUTH] Validating tags");
            self.client
                .get(count_endpoint)
                .basic_auth(&self.auth.username, Some(&self.auth.api_key))
        } else {
            debug!("Validating tags");
            self.client.get(count_endpoint)
        };

        let count = request.send().await?.json::<Value>().await?;

        if let Some(count) = count["counts"]["posts"].as_u64() {
            // Bail out if no posts are found
            if count == 0 {
                return Err(ExtractorError::ZeroPosts);
            }

            debug!("Found {} posts", count);
            Ok(())
        } else {
            Err(ExtractorError::InvalidServerResponse)
        }
    }

    async fn get_post_list(&self, page: usize) -> Result<Vec<Post>, ExtractorError> {
        let url = format!(
            "{}?tags={}",
            ImageBoards::Danbooru.post_url(),
            &self.tag_string
        );

        // Fetch item list from page
        let req = if self.auth_state {
            debug!("[AUTH] Fetching posts from page {}", page);
            self.client
                .get(url)
                .query(&[("page", page), ("limit", 200)])
                .basic_auth(&self.auth.username, Some(&self.auth.api_key))
        } else {
            debug!("Fetching posts from page {}", page);
            self.client
                .get(url)
                .query(&[("page", page), ("limit", 200)])
        };

        let post_array = req.send().await?.json::<Value>().await?;

        let start_point = Instant::now();
        let posts: Vec<Post> = post_array
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["file_url"].as_str().is_some())
            .map(|c| {
                let mut tag_list = AHashSet::new();

                for i in c["tag_string"].as_str().unwrap().split(' ') {
                    tag_list.insert(i.to_string());
                }

                let rt = c["rating"].as_str().unwrap();
                let rating = if rt == "s" {
                    Rating::Questionable
                } else {
                    Rating::from_str(rt)
                };

                Post {
                    id: c["id"].as_u64().unwrap(),
                    md5: c["md5"].as_str().unwrap().to_string(),
                    url: c["file_url"].as_str().unwrap().to_string(),
                    extension: c["file_ext"].as_str().unwrap().to_string(),
                    tags: tag_list,
                    rating,
                }
            })
            .collect();
        let end_iter = Instant::now();

        debug!("List size: {}", posts.len());
        debug!("Post mapping took {:?}", end_iter - start_point);
        Ok(posts)
    }
}