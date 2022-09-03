//! All internal logic for interacting with and downloading from imageboard websites.
use crate::imageboards::auth::ImageboardConfig;
use crate::progress_bars::BarTemplates;
use bincode::deserialize;
use clap::ValueEnum;
use colored::Colorize;
use directories::ProjectDirs;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use std::{fs::create_dir_all, io, path::PathBuf};
use tokio::fs::{read, remove_file};

use self::auth::AuthError;

pub mod auth;
pub mod extractors;
pub mod post;
pub mod queue;

/// All currently supported imageboards and their underlying attributes
#[derive(Debug, Copy, Clone, Ord, PartialOrd, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum ImageBoards {
    /// Represents the website ```https://danbooru.donmai.us``` or it's safe variant ```https://safebooru.donmai.us```.
    Danbooru,
    /// Represents the website ```https://e621.net``` or it's safe variant ```https://e926.net```.
    E621,
    /// Represents the website ```https://rule34.xxx```
    Rule34,
    /// Represents the website ```http://realbooru.com```
    Realbooru,
    /// Represents the website ```https://konachan.com``` or it's safe variant ```https://konachan.net```.
    Konachan,
    /// Represents the website ```https://gelbooru.com```.
    Gelbooru,
}

impl ToString for ImageBoards {
    fn to_string(&self) -> String {
        match self {
            ImageBoards::Danbooru => String::from("danbooru"),
            ImageBoards::E621 => String::from("e621"),
            ImageBoards::Rule34 => String::from("rule34"),
            ImageBoards::Realbooru => String::from("realbooru"),
            ImageBoards::Konachan => String::from("konachan"),
            ImageBoards::Gelbooru => String::from("gelbooru"),
        }
    }
}

impl ImageBoards {
    /// Each variant can generate a specific user-agent to connect to the imageboard site.
    ///
    /// It will always follow the version declared inside ```Cargo.toml```
    #[inline]
    pub fn user_agent(self) -> String {
        let app_name = "Rust Imageboard Downloader";
        let variant = match self {
            ImageBoards::Danbooru => " (by danbooru user FerrahWolfeh)",
            ImageBoards::E621 => " (by e621 user FerrahWolfeh)",
            _ => "",
        };
        let ua = format!("{}/{}{}", app_name, env!("CARGO_PKG_VERSION"), variant);
        debug!("Using user-agent: {}", ua);
        ua
    }

    /// Exclusive to ```ImageBoards::Danbooru```.
    ///
    /// Will return ```Some``` with the endpoint for the total post count with given tags. In case it's used with another variant, it returns ```None```.
    ///
    /// The ```safe``` bool will determine if the endpoint directs to ```https://danbooru.donmai.us``` or ```https://safebooru.donmai.us```.
    #[inline]
    pub fn post_count_url(self) -> Option<&'static str> {
        match self {
            ImageBoards::Danbooru => Some("https://danbooru.donmai.us/counts/posts.json"),
            _ => None,
        }
    }

    /// Returns the endpoint for the post list with their respective tags.
    #[inline]
    pub fn post_url(&self) -> &'static str {
        match self {
            ImageBoards::Danbooru => "https://danbooru.donmai.us/posts.json",
            ImageBoards::E621 => "https://e621.net/posts.json",
            ImageBoards::Rule34 => {
                "https://api.rule34.xxx/index.php?page=dapi&s=post&q=index&json=1"
            }
            ImageBoards::Konachan => "https://konachan.com/post.json",
            ImageBoards::Realbooru => {
                "http://realbooru.com/index.php?page=dapi&s=post&q=index&json=1"
            }
            ImageBoards::Gelbooru => {
                "http://gelbooru.com/index.php?page=dapi&s=post&q=index&json=1"
            }
        }
    }

    /// Returns `true` if the imageboard has a user-defined blacklist and thus, authentication support.
    #[inline]
    pub fn has_native_blacklist(self) -> bool {
        matches!(self, Self::Danbooru | Self::E621)
    }

    /// Returns max number of posts per page a imageboard can have
    #[inline]
    pub fn max_post_limit(self) -> usize {
        match self {
            ImageBoards::Danbooru => 200,
            ImageBoards::E621 => 320,
            ImageBoards::Rule34 => 1000,
            ImageBoards::Realbooru => 1000,
            ImageBoards::Konachan => 100,
            ImageBoards::Gelbooru => 100,
        }
    }

    /// Returns special-themed progress bar templates for each variant
    #[inline]
    pub fn progress_template(self) -> BarTemplates {
        match self {
            ImageBoards::E621 => BarTemplates {
                main: "{spinner:.yellow.bold} {elapsed_precise:.bold} {wide_bar:.blue/white.dim} {percent:.bold}  {pos:.yellow} (eta. {eta})",
                download: "{spinner:.blue.bold} {bar:40.yellow/white.dim} {percent:.bold} | {byte_progress:21.blue} @ {bytes_per_sec:>13.yellow} (eta. {eta:<4.blue})",
            },
            ImageBoards::Realbooru => BarTemplates {
                main: "{spinner:.red.bold} {elapsed_precise:.bold} {wide_bar:.red/white.dim} {percent:.bold}  {pos:.bold} (eta. {eta})", 
                download: "{spinner:.red.bold} {bar:40.red/white.dim} {percent:.bold} | {byte_progress:21.bold.green} @ {bytes_per_sec:>13.red} (eta. {eta:<4})",
            },
            _ => BarTemplates::default(),
        }
    }

    /// Returns the url used for validating the login input and parsing the user`s profile.
    #[inline]
    pub fn auth_url(self) -> &'static str {
        match self {
            ImageBoards::Danbooru => "https://danbooru.donmai.us/profile.json",
            ImageBoards::E621 => "https://e621.net/users/",
            _ => "",
        }
    }

    /// Returns a `PathBuf` pointing to the imageboard's authentication cache.
    ///
    /// This is XDG-compliant and saves cache files to
    /// `$XDG_CONFIG_HOME/imageboard-downloader/<imageboard>` on Linux or
    /// `%APPDATA%/FerrahWolfeh/imageboard-downloader/<imageboard>` on Windows
    #[inline]
    pub fn auth_cache_dir() -> Result<PathBuf, io::Error> {
        let cdir = ProjectDirs::from("com", "FerrahWolfeh", "imageboard-downloader").unwrap();

        let cfold = cdir.config_dir();

        if !cfold.exists() {
            create_dir_all(cfold)?;
        }

        Ok(cfold.to_path_buf())
    }

    /// Reads and parses the authentication cache from the path provided by `auth_cache_dir`.
    ///
    /// Returns `None` if the file is corrupted or does not exist.
    pub async fn read_config_from_fs(&self) -> Result<Option<ImageboardConfig>, AuthError> {
        let cfg_path = Self::auth_cache_dir()?.join(PathBuf::from(self.to_string()));
        if let Ok(config_auth) = read(&cfg_path).await {
            debug!("Authentication cache found");

            if let Ok(decompressed) = zstd::decode_all(config_auth.as_slice()) {
                debug!("Authentication cache decompressed.");
                return if let Ok(rd) = deserialize::<ImageboardConfig>(&decompressed) {
                    debug!("Authentication cache decoded.");
                    debug!("User id: {}", rd.user_data.id);
                    debug!("Username: {}", rd.user_data.name);
                    debug!("Blacklisted tags: '{:?}'", rd.user_data.blacklisted_tags);
                    Ok(Some(rd))
                } else {
                    warn!(
                        "{}",
                        "Auth cache is invalid or empty. Running without authentication"
                    );
                    Ok(None)
                };
            }
            debug!("Failed to decompress authentication cache.");
            debug!("Removing corrupted file");
            remove_file(cfg_path).await?;
            error!(
                "{}",
                "Auth cache is corrupted. Please authenticate again."
                    .bold()
                    .red()
            );
        };
        debug!("Running without authentication");
        Ok(None)
    }
}
