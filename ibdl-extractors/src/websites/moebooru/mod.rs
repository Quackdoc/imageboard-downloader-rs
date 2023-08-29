//! Post extractor for `https://konachan.com` and other Moebooru imageboards
use async_trait::async_trait;
use ibdl_common::post::extension::Extension;
use ibdl_common::post::tags::{Tag, TagType};
use ibdl_common::reqwest::Client;
use ibdl_common::{
    client, extract_ext_from_url, join_tags,
    log::debug,
    post::{rating::Rating, Post, PostQueue},
    serde_json,
    tokio::time::Instant,
    ImageBoards,
};
use std::fmt::Display;

use crate::{
    blacklist::BlacklistFilter, error::ExtractorError, websites::moebooru::models::KonachanPost,
};

use super::Extractor;

mod models;
mod unsync;

pub struct MoebooruExtractor {
    client: Client,
    tags: Vec<String>,
    tag_string: String,
    download_ratings: Vec<Rating>,
    disable_blacklist: bool,
    total_removed: u64,
    map_videos: bool,
    excluded_tags: Vec<String>,
    selected_extension: Option<Extension>,
}

#[async_trait]
impl Extractor for MoebooruExtractor {
    fn new<S>(
        tags: &[S],
        download_ratings: &[Rating],
        disable_blacklist: bool,
        map_videos: bool,
    ) -> Self
    where
        S: ToString + Display,
    {
        // Use common client for all connections with a set User-Agent
        let client = client!(ImageBoards::Konachan);

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
            download_ratings: download_ratings.to_vec(),
            disable_blacklist,
            total_removed: 0,
            map_videos,
            excluded_tags: vec![],
            selected_extension: None,
        }
    }

    async fn search(&mut self, page: u16) -> Result<PostQueue, ExtractorError> {
        let mut posts = Self::get_post_list(self, page).await?;

        if posts.is_empty() {
            return Err(ExtractorError::ZeroPosts);
        }

        posts.sort();
        posts.reverse();

        let qw = PostQueue {
            imageboard: ImageBoards::Konachan,
            client: self.client.clone(),
            posts,
            tags: self.tags.clone(),
        };

        Ok(qw)
    }

    async fn full_search(
        &mut self,
        start_page: Option<u16>,
        limit: Option<u16>,
    ) -> Result<PostQueue, ExtractorError> {
        let blacklist = BlacklistFilter::new(
            ImageBoards::Konachan,
            &Vec::default(),
            &self.download_ratings,
            self.disable_blacklist,
            !self.map_videos,
            self.selected_extension,
        )
        .await?;

        let mut fvec = if let Some(size) = limit {
            Vec::with_capacity(size as usize)
        } else {
            Vec::with_capacity(100)
        };

        let mut page = 1;

        loop {
            let position = if let Some(n) = start_page {
                page + n
            } else {
                page
            };

            let posts = Self::get_post_list(self, position).await?;
            let size = posts.len();

            if size == 0 {
                break;
            }

            let mut list = if !self.disable_blacklist || !self.download_ratings.is_empty() {
                let (removed, posts) = blacklist.filter(posts);
                self.total_removed += removed;
                posts
            } else {
                posts
            };

            fvec.append(&mut list);

            if let Some(num) = limit {
                if fvec.len() >= num as usize {
                    break;
                }
            }

            if size < 100 || page == 100 {
                break;
            }

            page += 1;
        }

        if fvec.is_empty() {
            return Err(ExtractorError::ZeroPosts);
        }

        fvec.sort();
        fvec.reverse();

        let fin = PostQueue {
            imageboard: ImageBoards::Konachan,
            client: self.client.clone(),
            posts: fvec,
            tags: self.tags.clone(),
        };

        Ok(fin)
    }

    fn force_extension(&mut self, extension: Extension) -> &mut Self {
        self.selected_extension = Some(extension);
        self
    }

    async fn get_post_list(&self, page: u16) -> Result<Vec<Post>, ExtractorError> {
        // Get URL
        let url = format!(
            "{}?tags={}",
            ImageBoards::Konachan.post_url(),
            &self.tag_string
        );

        let items = self
            .client
            .get(&url)
            .query(&[("page", page), ("limit", 100)])
            .send()
            .await?
            .text()
            .await?;

        let start = Instant::now();

        let post_list = self.map_posts(items)?;

        let end = Instant::now();

        debug!("List size: {}", post_list.len());
        debug!("Post mapping took {:?}", end - start);

        Ok(post_list)
    }

    fn map_posts(&self, raw_json: String) -> Result<Vec<Post>, ExtractorError> {
        let items = serde_json::from_str::<Vec<KonachanPost>>(raw_json.as_str()).unwrap();

        let post_iter = items.iter().filter(|c| c.file_url.is_some());

        let mut post_mtx: Vec<Post> = Vec::with_capacity(post_iter.size_hint().0);

        post_iter.for_each(|c| {
            let url = c.file_url.clone().unwrap();

            let tag_iter = c.tags.split(' ');

            let mut tags = Vec::with_capacity(tag_iter.size_hint().0);

            let ext = extract_ext_from_url!(url);

            tag_iter.for_each(|i| {
                tags.push(Tag::new(i, TagType::Any));
            });

            let unit = Post {
                id: c.id.unwrap(),
                website: ImageBoards::Konachan,
                url,
                md5: c.md5.clone().unwrap(),
                extension: ext,
                tags,
                rating: Rating::from_rating_str(&c.rating),
            };

            post_mtx.push(unit);
        });

        Ok(post_mtx)
    }

    fn client(&self) -> Client {
        self.client.clone()
    }

    fn total_removed(&self) -> u64 {
        self.total_removed
    }

    fn imageboard(&self) -> ImageBoards {
        ImageBoards::Konachan
    }

    fn exclude_tags(&mut self, tags: &[String]) -> &mut Self {
        self.excluded_tags = tags.to_vec();
        self
    }
}
