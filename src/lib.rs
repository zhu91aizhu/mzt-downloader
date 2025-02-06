use std::collections::HashMap;
use std::fmt::Write;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Output;
use std::str::FromStr;
use std::string::ToString;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use lru::LruCache;
use reqwest::Client;
use scraper::{ElementRef, Html, Selector};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::{error, info};

use crate::util::filenamify;

async fn get_url_content(client: Client, url: &str) -> Result<String> {
    let response = client.get(url).send().await?;
    let response = response.error_for_status()?;
    let content = response.text().await?;
    Ok(content.to_string())
}

#[derive(Clone)]
pub struct Album {
    client: Client,
    pub name: String,
    url: String,
    parser: Arc<dyn Parser>
}

impl Album {

    async fn download_picture(&self, client: Client, parser: Arc<dyn Parser>, url: &str, save_to_path: PathBuf) -> Result<()> {
        let response = client.get(url).send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("send get picture request error: {}", response.status()))
        }

        let picture_name = self.parser.get_picture_name(url)?;
        let path = save_to_path.join(picture_name);
        let bytes = response.bytes().await?;
        let mut file = File::create(path).await?;
        file.write_all(&bytes).await?;

        Ok(())
    }

    async fn download_pictures(self: Arc<Self>, parser: Arc<dyn Parser>, save_to_path: &str) -> Result<()> {
        let pictures = parser.get_all_pictures(self.url.clone()).await?;
        let name = filenamify(&self.name, "");
        let path = Path::new(save_to_path).join(name);
        tokio::fs::create_dir_all(&path).await?;

        let pb = Arc::new(ProgressBar::new(pictures.len() as u64));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .with_key("eta", |state: &ProgressState, w: &mut dyn Write| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
            .progress_chars("#>-"));

        let semaphore = Arc::new(Semaphore::new(16));
        let mut tasks = vec![];
        for url in pictures {
            let permit = semaphore.clone().acquire_owned().await?;

            let base_path = path.clone();
            let pb = pb.clone();
            let client = self.client.clone();
            let p = parser.clone();
            let it = Arc::clone(&self);
            let task = tokio::task::spawn(async move {
                match it.download_picture(client, p, &url, base_path).await {
                    Ok(_) => {
                        pb.inc(1);
                        info!("picture {url} downloaded.");
                    },
                    Err(err) => {
                        error!("download picture {} error: {:?}", url, err);
                        println!("下载图片失败，详情请查看日志");
                    }
                }

                drop(permit);
            });

            tasks.push(task);
        }

        for task in tasks {
            if let Err(err) = task.await {
                error!("download picture task error: {:?}", err);
                println!("下载图片失败，详情请查看日志");
            }
        }

        pb.finish_with_message("下载完成");
        Ok(())
    }
}

pub type AlbumResult<'a> = Result<Option<&'a Vec<Album>>>;

pub mod parser {
    use std::sync::Arc;

    use anyhow::{anyhow, Result};

    use crate::{DiLi360Parser, MZTParser, Parser};

    pub fn parse(parser_code: &str) -> Result<Arc<dyn Parser>> {
        match parser_code.to_uppercase().as_str() {
            DiLi360Parser::PARSER_CODE => {
                Ok(Arc::new(DiLi360Parser::new()))
            }
            MZTParser::PARSER_CODE => {
                Ok(Arc::new(MZTParser::new()))
            }
            _ => Err(anyhow!("不支持的解析器: {}", parser_code))
        }
    }

    pub fn default_parser() -> Arc<dyn Parser> {
        Arc::new(DiLi360Parser::new())
    }
}

#[async_trait]
pub trait Parser: Send + Sync {

    fn parser_name(&self) -> String;

    fn parse_page_count(&self, document: &Html) -> Result<u32>;

    async fn parse_albums(&self, keyword: String, page: u32, size: u32) -> Result<(Vec<Album>, u32)>;

    fn get_pagination(&self, html: &str) -> usize;

    async fn get_page_pictures(&self, url: String) -> Result<Vec<String>>;

    async fn get_all_pictures(&self, url: String) -> Result<Vec<String>>;

    fn get_picture_name(&self, url: &str) -> Result<String>;

}

#[derive(Clone)]
struct DiLi360Parser {
    client: Client,
    page: u32,
    page_count: u32
}

impl DiLi360Parser {

    const PARSER_CODE: &'static str = "DILI360";

    const PARSER_NAME: &'static str = "中国地理";

    fn new() -> Self {
        Self {
            client: Client::new(),
            page: 0,
            page_count: 0
        }
    }
}

#[async_trait]
impl Parser for DiLi360Parser {

    fn parser_name(&self) -> String {
        DiLi360Parser::PARSER_NAME.to_string()
    }

    fn parse_page_count(&self, document: &Html) -> Result<u32> {
        let selector = Selector::parse("#pageFooter>a").map_err(|err| {
            anyhow!("parse selector error: {err:?}")
        })?;

        let element: Vec<ElementRef> = document.select(&selector).into_iter().collect();
        Ok(element.len() as u32)
    }

    async fn parse_albums(&self, keyword: String, page: u32, size: u32) -> Result<(Vec<Album>, u32)> {
        // 地理 360 搜索结果页面从 0 开始
        let url = format!("https://zhannei.baidu.com/cse/site?q={}&p={}&nsid=&cc=www.dili360.com", &keyword, page - 1);
        let html = get_url_content(self.client.clone(), &url).await?;
        let document = Html::parse_document(&html);
        let selector = Selector::parse("#results>div>h3>a").map_err(|err| {
            anyhow!("parse selector error: {err:?}")
        })?;

        let albums = document.select(&selector).into_iter().map(|element| {
            let href = element.value().attr("href");
            let texts = element.text().collect::<Vec<_>>();
            (href, texts)
        }).filter_map(|(href, texts)| {
            if href.is_none() || texts.is_empty() {
                None
            } else {
                let url = href.unwrap().to_string();
                let name = texts.join("");
                Some(Album {
                    client: self.client.clone(),
                    name,
                    url,
                    parser: Arc::new(self.clone())
                })
            }
        }).collect();

        let page_count = if self.page_count == 0 {
            self.parse_page_count(&document)?
        } else {
            self.page_count
        };

        Ok((albums, page_count))
    }

    fn get_pagination(&self, html: &str) -> usize {
        1
    }

    async fn get_page_pictures(&self, url: String) -> Result<Vec<String>> {
        let html = get_url_content(self.client.clone(), &url).await?;
        let document = Html::parse_document(&html);
        let selector = Selector::parse(".imgbox>.img>img").map_err(|err| {
            anyhow!("parse selector error: {err:?}")
        })?;

        let pictures: Vec<String> = document.select(&selector).into_iter().filter_map(|element| {
            if let Some(url) = element.value().attr("src") {
                Some(url.to_string())
            } else {
                None
            }
        }).collect();
        Ok(pictures)
    }

    async fn get_all_pictures(&self, url: String) -> Result<Vec<String>> {
        let pictures = self.get_page_pictures(url).await?;
        Ok(pictures)
    }

    fn get_picture_name(&self,  url: &str) -> Result<String> {
        let path = Path::new(url);
        if let Some(file_name) = path.file_name() {
            file_name.to_str().map(|s| {
                s.to_string()
            }).ok_or(anyhow!("get file name error: {url}"))
        } else {
            Err(anyhow!("get file name error: {url}"))
        }
    }

}

#[derive(Clone)]
struct MZTParser {
    client: Client,
    page: u32,
    page_count: u32
}

impl MZTParser {

    const PARSER_CODE: &'static str = "MZT";

    const PARSER_NAME: &'static str = "妹子图";

    fn new() -> Self {
        Self {
            client: Client::new(),
            page: 0,
            page_count: 0
        }
    }
}

#[async_trait]
impl Parser for MZTParser {

    fn parser_name(&self) -> String {
        MZTParser::PARSER_NAME.to_string()
    }

    fn parse_page_count(&self, document: &Html) -> Result<u32> {
        todo!()
    }

    async fn parse_albums(&self, keyword: String, page: u32, size: u32) -> Result<(Vec<Album>, u32)> {
        todo!()
    }

    fn get_pagination(&self, html: &str) -> usize {
        todo!()
    }

    async fn get_page_pictures(&self, url: String) -> Result<Vec<String>> {
        todo!()
    }

    async fn get_all_pictures(&self, url: String) -> Result<Vec<String>> {
        todo!()
    }

    fn get_picture_name(&self, url: &str) -> Result<String> {
        todo!()
    }
}

pub struct AlbumSearcher {
    parser: Arc<dyn Parser>,
    page: u32,
    page_count: u32,
    size: u32,
    keyword: String,
    albums: LruCache<String, Vec<Album>>
}

impl AlbumSearcher {

    pub const DEFAULT_PAGE_SIZE: u32 = 10u32;

    pub fn new(parser: Arc<dyn Parser>, keyword: &str, size: u32) -> Self {
        let mut size = size;
        if size < 1 {
            size = Self::DEFAULT_PAGE_SIZE;
        }

        Self {
            parser,
            page: 0,
            page_count: 0,
            size,
            keyword: keyword.to_string(),
            albums: LruCache::new(NonZeroUsize::new(64).unwrap())
        }
    }

    async fn get_albums(&mut self) -> AlbumResult {
        let key = format!("page-{}", &self.page);
        if self.albums.contains(&key) {
            Ok(self.albums.get(&key))
        } else {
            // 获取新数据
            let (albums, page_count) = self.parser.parse_albums(
                self.keyword.clone(), self.page, self.size).await?;
            // page_count 表示第一次获取数据，总页数没有赋值
            // 有些网站不能获取到总页数，通过每次获取数据时，更新页码总数
            if self.page_count == 0 || self.page_count < page_count {
                self.page_count = page_count;
            }

            self.albums.push(key.clone(), albums);
            Ok(self.albums.get(&key))
        }
    }

    pub async fn current(&mut self) -> AlbumResult {
        if self.page_count == 0 {
            // 当搜索器初始化后，分页总数未被初始化
            self.page = 1;
        }

        self.get_albums().await
    }

    pub async fn prev(&mut self) -> AlbumResult {
        if self.page > 1 {
            self.page -= 1;
        } else {
            // 当搜索器初始化后，分页总数未被初始化
            self.page = 1;
        }

        self.get_albums().await
    }

    pub async fn next(&mut self) -> AlbumResult {
        if self.page_count == 0 {
            // 当搜索器初始化后，分页总数未被初始化
            self.page = 1;
        } else if self.page < self.page_count {
            self.page += 1;
        } else {
            self.page_count;
        }

        self.get_albums().await
    }

    pub async fn first(&mut self) -> AlbumResult {
        self.page = 1;
        self.get_albums().await
    }

    pub async fn last(&mut self) -> AlbumResult {
        if self.page_count == 0 {
            // 解析第一页内容，并获取分页总数
            self.next().await?;
        }

        self.page = self.page_count;
        self.get_albums().await
    }

    pub async fn download(&mut self, idx: usize) -> Result<()> {
        if self.page_count == 0 {
            return Err(anyhow!("no data"));
        }

        if self.page == 0 {
            return Err(anyhow!("no data"));
        }

        if idx == 0 {
            return Err(anyhow!("error album index"));
        }

        let key = format!("page-{}", self.page);
        let albums = self.albums.get(&key);
        if let Some(albums) = albums {
            if idx > albums.len() {
                return Err(anyhow!("error album index, max index: {}", albums.len()));
            }

            let index = idx - 1;
            let album = &albums[index];
            info!("download searcher {} page {} index album, album: {}", self.page, idx, album.name);
            let parser = self.parser.clone();
            let a = Arc::new(album.clone());
            a.download_pictures(parser.clone(), "./albums/").await
        } else {
            Err(anyhow!("current page no data"))
        }
    }
}

mod util {
    use lazy_static::lazy_static;
    use regex::Regex;

    lazy_static! {
        static ref RESERVED: Regex =
            Regex::new("[<>:\"/\\\\|?*\u{0000}-\u{001F}\u{007F}\u{0080}-\u{009F}]+").unwrap();
        static ref WINDOWS_RESERVED: Regex = Regex::new("^(con|prn|aux|nul|com\\d|lpt\\d)$").unwrap();
        static ref OUTER_PERIODS: Regex = Regex::new("^\\.+|\\.+$").unwrap();
    }

    pub(super) fn filenamify<S: AsRef<str>>(input: S, replacement: &str) -> String {
        let input = RESERVED.replace_all(input.as_ref(), replacement);
        let input = OUTER_PERIODS.replace_all(input.as_ref(), replacement);

        let mut result = input.into_owned();
        if WINDOWS_RESERVED.is_match(result.as_str()) {
            result.push_str(replacement);
        }

        result
    }

}

#[cfg(test)]
mod tests {
    use tokio;

    use super::*;

    #[test]
    fn test_download_album() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let parser = parser::default_parser();
            let mut searcher = AlbumSearcher::new(parser, "云南", AlbumSearcher::DEFAULT_PAGE_SIZE);
            let ret = searcher.next().await;
            let ret = searcher.next().await;
            assert!(ret.is_ok());

            let opt = ret.unwrap();
            assert!(opt.is_some());

            let albums = opt.unwrap();
            assert_eq!(albums.len(), 10usize);

            match searcher.download(6).await {
                Ok(_) => {
                    println!("album downloaded.");
                }
                Err(err) => {
                    println!("download album error: {err:?}");
                }
            }
        });
    }

}
