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
use reqwest::{Client, header};
use scraper::{ElementRef, Html, Selector};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::{error, info};
use pinyin::ToPinyin;
use reqwest::header::{HeaderMap, HeaderValue};
use crate::util::filenamify;

async fn get_url_content(client: Client, url: &str, encoding: Option<String>, headers: Option<HeaderMap>) -> Result<String> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(header::USER_AGENT, HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"));
    default_headers.insert(header::ACCEPT, HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,image/apng,*/*;q=0.8"));
    default_headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    default_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate, br"));
    default_headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    default_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=0"));

    let mut request = client.get(url);
    if let Some(headers) = headers {
        for (n, v) in headers {
            if let Some(name) = n {
                default_headers.insert(name, v);
            }
        }
        request = request.headers(default_headers);
    }

    let response = request.send().await?;
    let response = response.error_for_status()?;

    let content = match encoding {
        Some(encode) => response.text_with_charset(&encode).await?,
        None => response.text().await?
    };

    Ok(content)
}

#[derive(Clone)]
pub struct Album {
    pub name: String,
    url: String
}

impl Album {

    async fn download_picture(&self, client: &Client, parser: &dyn Parser, url: &str, save_to_path: PathBuf) -> Result<()> {
        let response = client.get(url).send().await.map_err(|e| {
            anyhow!("Failed to send request for {}: {}", url, e)
        })?;

        let picture_name = parser.get_picture_name(url)?;
        let path = save_to_path.join(picture_name);
        let bytes = response.bytes().await?;
        let mut file = File::create(path).await?;
        file.write_all(&bytes).await?;

        Ok(())
    }

    async fn download_pictures(self: Arc<Self>, client: &Client, parser: Arc<dyn Parser>, save_to_path: &str) -> Result<()> {
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
            let client = client.clone();
            let p = parser.clone();
            let it = Arc::clone(&self);
            let task = tokio::task::spawn(async move {
                match it.download_picture(&client, &*p, &url, base_path).await {
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

    use crate::{DiLi360Parser, SFTKParser, Parser};

    pub fn parse(parser_code: &str) -> Result<Arc<dyn Parser>> {
        match parser_code.to_uppercase().as_str() {
            DiLi360Parser::PARSER_CODE => {
                Ok(Arc::new(DiLi360Parser::new()))
            }
            SFTKParser::PARSER_CODE => {
                Ok(Arc::new(SFTKParser::new()))
            }
            _ => Err(anyhow!("不支持的解析器: {}", parser_code))
        }
    }

    pub fn default_parser() -> Arc<dyn Parser> {
        Arc::new(DiLi360Parser::new())
    }

    pub fn parsers() -> Vec<(String, String)> {
        let mut parsers = vec![];
        parsers.push((DiLi360Parser::PARSER_CODE.to_string(), DiLi360Parser::PARSER_NAME.to_string()));
        parsers.push((SFTKParser::PARSER_CODE.to_string(), SFTKParser::PARSER_NAME.to_string()));
        parsers
    }

}

#[derive(Clone)]
struct InnerParser {
    client: Client,
    page: u32,
    page_count: u32
}

impl InnerParser {
    fn new() -> Self {
        Self {
            client: Client::new(),
            page: 0,
            page_count: 0
        }
    }

    async fn get_page_pictures(&self, url: String, selector: &str, encoding: Option<String>, headers: Option<HeaderMap>) -> Result<Vec<String>> {
        let html = get_url_content(self.client.clone(), &url, encoding, headers).await?;
        let document = Html::parse_document(&html);
        let selector = Selector::parse(selector).map_err(|err| {
            anyhow!("parse page pictures selector error: {err:?}")
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
}

#[async_trait]
pub trait Parser: Send + Sync {

    fn parser_name(&self) -> String;

    fn client(&self) -> Arc<&Client>;

    fn parse_page_count(&self, document: &Html) -> Result<u32>;

    async fn parse_albums(&self, keyword: String, page: u32, size: u32) -> Result<(Vec<Album>, u32)>;

    fn get_pagination(&self, html: &str) -> usize;

    async fn get_page_pictures(&self, url: String) -> Result<Vec<String>>;

    async fn get_all_pictures(&self, url: String) -> Result<Vec<String>>;

    fn get_picture_name(&self, url: &str) -> Result<String>;

}

#[derive(Clone)]
struct DiLi360Parser {
    inner: InnerParser
}

impl DiLi360Parser {

    const PARSER_CODE: &'static str = "DILI360";

    const PARSER_NAME: &'static str = "中国地理";

    fn new() -> Self {
        Self {
            inner: InnerParser::new()
        }
    }
}

#[async_trait]
impl Parser for DiLi360Parser {

    fn parser_name(&self) -> String {
        DiLi360Parser::PARSER_NAME.to_string()
    }

    fn client(&self) -> Arc<&Client> {
        Arc::new(&self.inner.client)
    }

    fn parse_page_count(&self, document: &Html) -> Result<u32> {
        let selector = Selector::parse("#pageFooter .pager-normal-foot").map_err(|err| {
            anyhow!("parse selector error: {err:?}")
        })?;

        let last_element = document.select(&selector).last();
        if last_element.is_none() {
            return Err(anyhow!("parse page count error: not found page element"));
        }

        let element = last_element.unwrap();
        let text = element.text().next();
        if text.is_none() {
            return Err(anyhow!("parse page count error: not found page text"));
        }

        let text = text.unwrap();
        let page_count = text.parse::<u32>().map_err(|e| {
            anyhow!("parse page count error: {e:?}")
        })?;
        Ok(page_count)
    }

    async fn parse_albums(&self, keyword: String, page: u32, size: u32) -> Result<(Vec<Album>, u32)> {
        // 地理 360 搜索结果页面从 0 开始
        let url = format!("https://zhannei.baidu.com/cse/site?q={}&p={}&nsid=&cc=www.dili360.com", &keyword, page - 1);
        let html = get_url_content(self.inner.client.clone(), &url, None, None).await?;
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
                    name,
                    url
                })
            }
        }).collect();

        let page_count = if self.inner.page_count == 0 {
            self.parse_page_count(&document)?
        } else {
            self.inner.page_count
        };

        Ok((albums, page_count))
    }

    fn get_pagination(&self, html: &str) -> usize {
        1
    }

    async fn get_page_pictures(&self, url: String) -> Result<Vec<String>> {
        self.inner.get_page_pictures(url, ".imgbox>.img>img", None, None).await
    }

    async fn get_all_pictures(&self, url: String) -> Result<Vec<String>> {
        let pictures = self.get_page_pictures(url).await?;
        Ok(pictures)
    }

    fn get_picture_name(&self,  url: &str) -> Result<String> {
        let path = Path::new(url);
        if let Some(file_name) = path.file_name() {
            file_name.to_str().map(|s| {
                let mut names = s.split("@");
                let name = names.next();
                name.unwrap().to_string()
            }).ok_or(anyhow!("get file name error: {url}"))
        } else {
            Err(anyhow!("get file name error: {url}"))
        }
    }

}

#[derive(Clone)]
struct SFTKParser {
    inner: InnerParser
}

impl SFTKParser {

    const PARSER_CODE: &'static str = "SFTK";

    const PARSER_NAME: &'static str = "私房图库";

    const BASE_URL: &'static str = "http://www.sftuku.com";

    fn new() -> Self {
        Self {
            inner: InnerParser::new()
        }
    }

    fn keyword_to_pinyin(keyword: &str) -> String {
        let pinyin: String = keyword.chars()
            .map(|c| c.to_pinyin().map(|p| p.plain().to_string()).unwrap_or(c.to_string()))
            .collect::<Vec<String>>()
            .join("");
        pinyin
    }

    fn default_headers() -> HeaderMap {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("zh-CN,zh-Hans;q=0.9"));
        default_headers.insert(header::HOST, HeaderValue::from_static("www.sftuku.com"));
        default_headers
    }
}

#[async_trait]
impl Parser for SFTKParser {

    fn parser_name(&self) -> String {
        SFTKParser::PARSER_NAME.to_string()
    }

    fn client(&self) -> Arc<&Client> {
        Arc::new(&self.inner.client)
    }

    fn parse_page_count(&self, document: &Html) -> Result<u32> {
        let selector = Selector::parse(".pagelist").map_err(|err| {
            anyhow!("parse selector error: {err:?}")
        })?;

        let elements: Vec<ElementRef> = document.select(&selector).into_iter().collect();
        Ok(elements.len() as u32)
    }

    async fn parse_albums(&self, keyword: String, page: u32, size: u32) -> Result<(Vec<Album>, u32)> {
        let pinyin = Self::keyword_to_pinyin(&keyword);
        let url = format!("http://www.sftuku.com/chis/{}/{}.html", &pinyin, page);
        let html = get_url_content(self.inner.client.clone(), &url, Some("GBK".to_string()), Some(Self::default_headers())).await?;
        println!("html: {}", html);
        let document = Html::parse_document(&html);
        let selector = Selector::parse("#list>ul>div>.title>a").map_err(|err| {
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
                let url = format!("{}{}", Self::BASE_URL, href.unwrap());
                let name = texts.join("");
                Some(Album {
                    name,
                    url
                })
            }
        }).collect();

        let page_count = if self.inner.page_count == 0 {
            self.parse_page_count(&document)?
        } else {
            self.inner.page_count
        };

        Ok((albums, page_count))
    }

    fn get_pagination(&self, html: &str) -> usize {
        let ret = Selector::parse(".pagelist>a");
        if ret.is_err() {
            error!("parse selector error: {:?}", ret.err());
            return 0;
        }

        let selector = ret.unwrap();
        let document = Html::parse_document(&html);
        let elements: Vec<ElementRef> = document.select(&selector).into_iter().collect();
        elements.len() + 1
    }

    async fn get_page_pictures(&self, url: String) -> Result<Vec<String>> {
        self.inner.get_page_pictures(url, "#picg>.slide>a>img", Some("GBK".to_string()), Some(Self::default_headers())).await
    }

    async fn get_all_pictures(&self, url: String) -> Result<Vec<String>> {
        let html = get_url_content(self.inner.client.clone(), &url, Some("GBK".to_string()), Some(Self::default_headers())).await?;
        let page_count = self.get_pagination(&html);
        let mut all_pictures = vec![];
        let base_url = &url[0..url.len() - 5];
        for i in 1..=page_count {
            let page_url = format!("{}_{}.html", base_url, i);
            let mut pictures = self.get_page_pictures(page_url).await?;
            all_pictures.append(&mut pictures);
        }

        Ok(all_pictures)
    }

    fn get_picture_name(&self, url: &str) -> Result<String> {
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

    pub fn page(&self) -> u32 {
        self.page
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
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

    pub async fn jump(&mut self, page: &u32) -> AlbumResult {
        let page = *page;
        self.page = if page < 1 {
            1
        } else {
            if self.page_count == 0 {
                // 解析第一页内容，并获取分页总数
                self.next().await?;
            }

           if self.page_count < page {
                self.page_count
            }
            else {
                page
            }
        };

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
            let client = parser.client();
            let a = Arc::new(album.clone());
            a.download_pictures(*client, parser.clone(), "./albums/").await
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

    #[test]
    fn test_keyword_to_pinyin() {
        let keyword = "左公子";
        let pinyin = SFTKParser::keyword_to_pinyin(keyword);
        assert_eq!(pinyin, "zuogongzi".to_string());

        let keyword = "左公子11";
        let pinyin = SFTKParser::keyword_to_pinyin(keyword);
        assert_eq!(pinyin, "zuogongzi11".to_string());
    }

}
