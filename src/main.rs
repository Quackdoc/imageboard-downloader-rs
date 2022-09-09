use anyhow::{bail, Error};
use bincode::{deserialize, serialize};
use clap::Parser;
use colored::Colorize;
use imageboard_downloader::*;
use log::debug;
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use tokio::{fs::remove_file, task::spawn_blocking};
use zstd::{decode_all, encode_all};

extern crate tokio;

#[derive(Parser, Debug)]
#[clap(name = "Imageboard Downloader", author, version, about, long_about = None)]
struct Cli {
    /// Tags to search
    #[clap(value_parser, required = true)]
    tags: Vec<String>,

    /// Specify which website to download from
    #[clap(short, long, arg_enum, ignore_case = true, default_value_t = ImageBoards::Danbooru)]
    imageboard: ImageBoards,

    /// Where to save downloaded files
    #[clap(
        short,
        long,
        parse(from_os_str),
        value_name = "PATH",
        help_heading = "SAVE"
    )]
    output: Option<PathBuf>,

    /// Number of simultaneous downloads
    #[clap(
        short = 'd',
        value_name = "NUMBER",
        value_parser(clap::value_parser!(u8).range(1..=20)),
        default_value_t = 3,
        help_heading = "DOWNLOAD"
    )]
    simultaneous_downloads: u8,

    /// Authenticate to the imageboard website.
    ///
    /// This flag only needs to be set a single time.
    ///
    /// Once authenticated, it's possible to use your blacklist to exclude posts with unwanted tags
    #[clap(short, long, action, help_heading = "GENERAL")]
    auth: bool,

    /// Download images from the safe version of the selected Imageboard.
    ///
    /// Currently only works with Danbooru, e621 and Konachan. This flag will be silently ignored if other imageboard is selected
    ///
    /// Useful if you only want to download posts with "safe" rating.
    #[clap(long, action, default_value_t = false, help_heading = "GENERAL")]
    safe_mode: bool,

    /// Save files with their ID as filename instead of it's MD5
    ///
    /// If the output dir has the same file downloaded with the MD5 name, it will be renamed to the post's ID
    #[clap(
        long = "id",
        value_parser,
        default_value_t = false,
        help_heading = "SAVE"
    )]
    save_file_as_id: bool,

    /// Limit max number of downloads
    #[clap(short, long, value_parser, help_heading = "DOWNLOAD")]
    limit: Option<usize>,

    /// Ignore both user and global blacklists
    #[clap(long, value_parser, default_value_t = false, help_heading = "GENERAL")]
    disable_blacklist: bool,

    /// Save posts inside a cbz file.
    ///
    /// Will always overwrite the destination file.
    #[clap(long, value_parser, default_value_t = false, help_heading = "SAVE")]
    cbz: bool,

    /// Select from which page to start scanning posts
    #[clap(
        short,
        long,
        value_parser,
        help_heading = "DOWNLOAD",
        value_name = "PAGE"
    )]
    start_page: Option<usize>,

    /// Download only the latest images for tag selection.
    ///
    /// Will not re-download already present and deleted images from folder
    #[clap(
        short,
        long,
        value_parser,
        default_value_t = false,
        help_heading = "SAVE"
    )]
    update: bool,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Cli = Cli::parse();
    env_logger::builder().format_timestamp(None).init();

    print!(
        "{}{}",
        "Scanning for posts, please wait".bold(),
        "...".bold().blink()
    );
    std::io::stdout().flush()?;

    let (mut post_queue, total_black, client) = match args.imageboard {
        ImageBoards::Danbooru => {
            let mut unit =
                DanbooruExtractor::new(&args.tags, args.safe_mode, args.disable_blacklist);
            unit.auth(args.auth).await?;
            let posts = unit.full_search(args.start_page, args.limit).await?;

            debug!("Collected {} valid posts", posts.posts.len());

            (posts, unit.total_removed(), unit.client())
        }
        ImageBoards::E621 => {
            let mut unit = E621Extractor::new(&args.tags, args.safe_mode, args.disable_blacklist);
            unit.auth(args.auth).await?;
            let posts = unit.full_search(args.start_page, args.limit).await?;

            debug!("Collected {} valid posts", posts.posts.len());

            (posts, unit.total_removed(), unit.client())
        }
        ImageBoards::Rule34 | ImageBoards::Realbooru | ImageBoards::Gelbooru => {
            let mut unit = GelbooruExtractor::new(&args.tags, false, args.disable_blacklist)
                .set_imageboard(args.imageboard)?;
            let posts = unit.full_search(args.start_page, args.limit).await?;

            debug!("Collected {} valid posts", posts.posts.len());

            (posts, unit.total_removed(), unit.client())
        }
        ImageBoards::Konachan => {
            let mut unit =
                MoebooruExtractor::new(&args.tags, args.safe_mode, args.disable_blacklist);
            let posts = unit.full_search(args.start_page, args.limit).await?;

            debug!("Collected {} valid posts", posts.posts.len());

            (posts, unit.total_removed(), unit.client())
        }
    };

    let last_post = post_queue
        .posts
        .iter()
        .max_by_key(|post| post.id)
        .unwrap()
        .clone();

    let place = match &args.output {
        None => std::env::current_dir()?,
        Some(dir) => dir.to_path_buf(),
    };

    let tgs = place.join(Path::new(&format!(
        "{}/{}/{}",
        args.imageboard.to_string(),
        &args.tags.join(" "),
        ".00_download_summary.bin"
    )));

    let odir = tgs.clone();

    if args.update && tgs.exists() {
        let last_post_downloaded: Result<Post, Error> = {
            let dsum = File::open(&tgs)?;

            let decomp = deserialize::<Post>(&decode_all(dsum)?)?;
            debug!("Latest post {:#?}", decomp);
            Ok(decomp)
        };
        if let Ok(post) = last_post_downloaded {
            post_queue.posts.retain(|c| c.id > post.id);
        } else {
            debug!("Summary file is corrupted, ignoring...");
            remove_file(&tgs).await?;
        }
    }

    if post_queue.posts.is_empty() {
        println!("\n{}", "No posts left to download!".bold());
        return Ok(());
    }

    let mut qw = Queue::new(
        args.imageboard,
        post_queue,
        args.simultaneous_downloads,
        Some(client),
        args.limit,
        args.cbz,
    );

    print!("\r");
    std::io::stdout().flush()?;

    let total_down = qw.download(args.output, args.save_file_as_id).await?;

    spawn_blocking(move || -> Result<(), Error> {
        let mut dsum = File::create(&odir)?;

        let string = match serialize(&last_post) {
            Ok(data) => encode_all(&*data, 9)?,
            Err(_) => bail!("Failed to serialize summary file"),
        };

        dsum.write_all(&string)?;
        Ok(())
    })
    .await
    .unwrap()?;

    println!(
        "{} {} {}",
        total_down.to_string().bold().blue(),
        "files".bold().blue(),
        "downloaded".bold()
    );

    if total_black > 0 {
        println!(
            "{} {}",
            total_black.to_string().bold().red(),
            "posts with blacklisted tags were not downloaded."
                .bold()
                .red()
        );
    }

    Ok(())
}
