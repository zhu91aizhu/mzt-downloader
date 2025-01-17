use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use reqwest::Client;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::{error, info};

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
    url: String
}

impl Album {

    fn get_picture_name(url: &str) -> Result<String> {
        let path = Path::new(url);
        if let Some(file_name) = path.file_name() {
            file_name.to_str().map(|s| {
                s.to_string()
            }).ok_or(anyhow!("get file name error: {url}"))
        } else {
            Err(anyhow!("get file name error: {url}"))
        }
    }

    fn get_pagination(&self, html: &str) -> usize {
        1usize
    }

    async fn get_page_pictures(&self, url: &str) -> Result<Vec<String>> {
        let html = get_url_content(self.client.clone(), url).await?;

        Ok(vec![
            "https://pics0.baidu.com/feed/adaf2edda3cc7cd91e66c08e7348fa31b90e91f2.jpeg".to_string(),
            "https://pics0.baidu.com/feed/38dbb6fd5266d016e23260f9db620f0934fa3594.jpeg".to_string(),
            "https://pics0.baidu.com/feed/29381f30e924b89998c589ff254fc69b087bf6d0.jpeg".to_string(),
        ])
    }

    async fn get_all_pictures(&self) -> Result<Vec<String>> {
        let html = get_url_content(self.client.clone(), "").await?;
        let page = self.get_pagination(&html);

        let mut all_pictures = vec![];
        for current in 1..=page {
            let url = format!("http://a.b.com/t/a_{current}.html");
            let mut pictures = self.get_page_pictures(&url).await?;
            all_pictures.append(&mut pictures);
        }

        Ok(all_pictures)
    }

    async fn download_picture(client: Client, url: &str, save_to_path: PathBuf) -> Result<()> {
        let response = client.get(url).send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("send get picture request error: {}", response.status()))
        }

        let picture_name = Self::get_picture_name(url)?;
        let path = save_to_path.join(picture_name);
        let bytes = response.bytes().await?;
        let mut file = File::create(path).await?;
        file.write_all(&bytes).await?;

        Ok(())
    }

    async fn download_pictures(&self, save_to_path: &str) -> Result<()> {
        let pictures = self.get_all_pictures().await?;

        let path = Path::new(save_to_path).join(&self.name);
        tokio::fs::create_dir_all(&path).await?;

        let pb = Arc::new(ProgressBar::new(pictures.len() as u64));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .with_key("eta", |state: &ProgressState, w: &mut dyn Write| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
            .progress_chars("#>-"));

        let mut tasks = vec![];
        for url in pictures {
            let base_path = path.clone();
            let client = self.client.clone();
            let pb = pb.clone();

            let task = tokio::spawn(async move {
                match Self::download_picture(client, &url, base_path).await {
                    Ok(_) => {
                        pb.inc(1);
                        info!("picture {url} downloaded.");
                    },
                    Err(err) => {
                        error!("download picture {} error: {:?}", url, err);
                        println!("下载图片失败，详情请查看日志");
                    }
                }
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

pub struct AlbumSearcher {
    client: Client,
    page: u32,
    page_count: u32,
    size: u32,
    keyword: String,
    albums: HashMap<String, Vec<Album>>
}

impl AlbumSearcher {
    pub const DEFAULT_PAGE_SIZE: u32 = 10u32;

    pub fn new(keyword: &str, size: u32) -> Self {
        let mut size = size;
        if size < 1 {
            size = 10;
        }

        Self {
            client: Client::new(),
            page: 0,
            page_count: 0,
            size,
            keyword: keyword.to_string(),
            albums: HashMap::new()
        }
    }

    fn parse_page_count(&self, html: &str) -> Result<u32> {
        Ok(1u32)
    }

    async fn parse_albums(&self) -> Result<(Vec<Album>, u32)> {
        let url = "";
        let html = get_url_content(self.client.clone(), url).await?;

        let page_count = if self.page_count == 0 {
            self.parse_page_count(&html)?
        } else {
            self.page_count
        };

        // TODO parse albums
        Ok((vec![], page_count))
    }

    async fn get_albums(&mut self) -> AlbumResult {
        let key = format!("page-{}", &self.page);
        if self.albums.contains_key(&key) {
            Ok(self.albums.get(&key))
        } else {
            // 获取新数据
            let (albums, page_count) = self.parse_albums().await?;
            if self.page_count == 0 {
                self.page_count = page_count;
            }
            self.albums.insert(key.clone(), albums);
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
            album.download_pictures("./").await
        } else {
            Err(anyhow!("current page no data"))
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio;
    use super::*;

    #[test]
    fn test_download_album() {
        let album = Album {
            client: Client::new(),
            name: "壁纸".to_string(),
            url: "none".to_string()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            match album.download_pictures("./").await {
                Ok(_) => {
                    println!("album {} downloaded.", &album.name);
                }
                Err(err) => {
                    println!("download album error: {err:?}");
                }
            }
        });
    }

}
