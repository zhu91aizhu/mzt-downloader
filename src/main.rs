use std::io::Write;
use std::str::FromStr;

use anyhow::anyhow;

use gqwht_download::{Album, AlbumSearcher};

#[derive(Debug)]
enum Command {
    HELP, SEARCH(String), FIRST, LAST, NEXT, PREV, DOWNLOAD(usize), QUIT, UNKNOWN, ArgumentErr(String)
}

impl FromStr for Command {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let input = s.trim().to_uppercase();
        let cmd = match input.as_str() {
            "HELP" | "H" => {
                Self::HELP
            }
            "FIRST" | "F" => {
                Self::FIRST
            }
            "LAST" | "L" => {
                Self::LAST
            }
            "NEXT" | "N" => {
                Self::NEXT
            }
            "PREV" | "P" => {
                Self::PREV
            }
            "QUIT" | "Q" => {
                Self::QUIT
            }
            other => {
                if other.starts_with("DOWNLOAD") || other.starts_with("D") {
                    let parts = other.split_whitespace();
                    match parts.skip(1).next() {
                        Some(idx) => {
                            match usize::from_str(idx) {
                                Ok(idx) => {
                                    Command::DOWNLOAD(idx)
                                }
                                Err(_) => {
                                    Self::ArgumentErr("参数必须为数字".to_string())
                                }
                            }
                        }
                        None => {
                            Self::ArgumentErr("缺少专辑索引参数".to_string())
                        }
                    }
                } else if other.starts_with("SEARCH") || other.starts_with("S") {
                    let parts = other.split_whitespace();
                    match parts.skip(1).next() {
                        Some(keyword) => {
                            Self::SEARCH(keyword.to_string())
                        }
                        None => {
                            Self::ArgumentErr("缺少专辑索引参数".to_string())
                        }
                    }
                } else {
                    Self::UNKNOWN
                }
            }
        };

        Ok(cmd)
    }
}

fn print_albums(albums: Option<&Vec<Album>>) {
    match albums {
        Some(albums) => {
            for (i, album) in albums.iter().enumerate() {
                println!("{}: {}", i + 1, album.name);
            }
        }
        None => {
            println!("no albums");
        }
    }
}

async fn get_albums(searcher: &mut Option<AlbumSearcher>, command: Command) {
    match searcher {
        Some(ref mut searcher) => {
            let ret = match &command {
                Command::FIRST => searcher.first().await,
                Command::LAST => searcher.last().await,
                Command::PREV => searcher.prev().await,
                Command::NEXT => searcher.next().await,
                _ => Err(anyhow!("not support command: {:?}", &command))
            };

            match ret {
                Ok(albums) => print_albums(albums),
                Err(err) => println!("get albums error: {err:?}")
            }
        }
        None => {
            println!("Please search for the album first");
        }
    }
}

#[tokio::main]
async fn main() {
    let mut searcher_opt = None;
    let mut searcher = &mut searcher_opt;

    loop {
        print!("-> ");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        if let Err(err) = std::io::stdin().read_line(&mut line) {
            println!("get input error: {}", err);
        }

        match FromStr::from_str(&line) {
            Ok(cmd) => {
                match cmd {
                    Command::HELP => {
                        println!("quit(q): quit tool");
                        println!("next(n): goto next page");
                        println!("prev(p): goto prev page");
                        println!("first(f): goto first page");
                        println!("last(l): goto last page");
                        println!("download [idx](d [idx]): download album");
                        println!("search [keyword](s [keyword]): search albums with keyword");
                    }
                    Command::SEARCH(keyword) => {
                        *searcher = Some(AlbumSearcher::new(&keyword, AlbumSearcher::DEFAULT_PAGE_SIZE));
                    }
                    Command::FIRST => {
                        get_albums(&mut searcher, Command::FIRST).await;
                    }
                    Command::LAST => {
                        get_albums(&mut searcher, Command::LAST).await;
                    }
                    Command::PREV => {
                        get_albums(&mut searcher, Command::PREV).await;
                    }
                    Command::NEXT => {
                        get_albums(&mut searcher, Command::NEXT).await;
                    }
                    Command::DOWNLOAD(idx) => {
                        match &mut searcher {
                            Some(ref mut searcher) => {
                                if let Err(err) = searcher.download(idx).await {
                                    println!("download error: {err:?}");
                                }
                            }
                            None =>{
                                println!("Please search for the album first");
                            }
                        }
                    }
                    Command::ArgumentErr(err) => {
                        println!("命令参数错误: {}", err);
                    }
                    Command::UNKNOWN => {
                        println!("未知的命令: {}", line.trim());
                    }
                    Command::QUIT => {
                        println!("bye bye.");
                        return;
                    }
                }
            }
            Err(err) => {
                println!("解析命令失败: {:?}", err);
            }
        }
    }

}
