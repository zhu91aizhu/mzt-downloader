use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use axum::{Json, Router, routing::get};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::error;
use tracing_subscriber::fmt::format;

use lmpic_downloader::{AlbumSearcher, parser};

#[derive(Clone)]
struct WebState {
    client: Client,
    parser_cache: Arc<DashMap<String, Arc<dyn parser::Parser>>>,
    searcher_cache: Arc<DashMap<String, AlbumSearcher>>
}

#[tokio::main]
async fn main() {
    let state = WebState {
        client: Client::new(),
        parser_cache: Arc::new(DashMap::new()),
        searcher_cache: Arc::new(DashMap::new())
    };

    let app = Router::new()
        .route("/album", get(album))
        .route("/album/parsers", get(get_parsers))
        .route("/album/search", get(search_albums))
        .route("/album/picture", get(forward_picture))
        .route("/album/pictures", get(get_album_by_url))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn album() -> Html<&'static str> {
    Html(include_str!("../../templates/index.html"))
}

#[derive(Serialize)]
struct Parser {
    code: String,
    name: String
}

#[derive(Serialize)]
struct CommonResponse<T> {
    code: i16,
    message: String,
    data: Option<T>
}

impl <T> CommonResponse<T> {
    fn success(data: T) -> CommonResponse<T> {
        CommonResponse {
            code: 0,
            message: "success".into(),
            data: Some(data)
        }
    }

    fn default_failure() -> CommonResponse<T> {
        CommonResponse {
            code: -1,
            message: "系统内部错误".into(),
            data: None
        }
    }

    fn failure(code: i16, message: String, data: T) -> CommonResponse<T> {
        CommonResponse {
            code,
            message,
            data: Some(data)
        }
    }
}

async fn get_parsers() -> Json<CommonResponse<Vec<Parser>>> {
    let parsers = parser::parsers();
    let parsers = parsers.into_iter().map(|p| {
        Parser {
            code: p.0,
            name: p.1
        }
    }).collect::<Vec<Parser>>();
    Json(CommonResponse::success(parsers))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub parser_code: String,
    pub keyword: String,
    pub page: u32,
    pub size: u32
}

#[derive(Serialize)]
struct Album {
    name: String,
    cover: String,
    url: String
}

async fn search_albums(Query(query): Query<SearchQuery>, State(state): State<WebState>) -> Json<CommonResponse<Vec<Album>>> {
    let parser = match parser::parse(&query.parser_code) {
        Ok(p) => p,
        Err(err) => {
            let error = format!("unknown parser: {}", query.parser_code);
            return Json(CommonResponse::failure(-1, error, vec![]));
        }
    };

    let searcher_key = format!("{}-{}", query.parser_code, query.keyword);
    let mut searcher = match state.searcher_cache.get_mut(&searcher_key) {
        Some(searcher) => searcher,
        None => {
            let searcher = AlbumSearcher::new(parser, &query.keyword, AlbumSearcher::DEFAULT_PAGE_SIZE);
            state.searcher_cache.insert(searcher_key.clone(), searcher);
            state.searcher_cache.get_mut(&searcher_key).unwrap()
        }
    };

    let result = searcher.jump(&query.page).await;
    let response = match result {
        Ok(albums) => {
            let albums = albums.unwrap_or(&vec![]).into_iter().map(|album| {
                Album {
                    name: album.name.clone(),
                    cover: album.cover.clone().unwrap_or("".to_string()),
                    url: album.url.clone()
                }
            }).collect::<Vec<Album>>();
            CommonResponse::success(albums)
        },
        Err(err) => {
            let error = format!("search error: {:?}", err);
            CommonResponse::failure(-1, error, vec![])
        }
    };
    Json(response)
}

#[derive(Deserialize)]
pub struct AlbumQuery {
    pub parser_code: String,
    pub url: String
}

async fn get_album_by_url(Query(query): Query<AlbumQuery>, State(state): State<WebState>) -> Json<CommonResponse<Vec<String>>> {
    let parser = match state.parser_cache.get(&query.parser_code) {
        Some(p) => p,
        None => {
            match parser::parse(&query.parser_code) {
                Ok(p) => {
                    state.parser_cache.insert(query.parser_code.clone(), p);
                    state.parser_cache.get(&query.parser_code).unwrap()
                }
                Err(err) => {
                    let error = format!("unknown parser: {}", query.parser_code);
                    return Json(CommonResponse::failure(-1, error, vec![]));
                }
            }
        }
    };

    let response =  match parser.get_all_pictures(query.url.clone()).await {
        Ok(pictures) => {
            let pictures = pictures.into_iter().map(|picture| {
                format!("/album/picture?url={}", picture)
            }).collect();
            CommonResponse::success(pictures)
        },
        Err(err) => {
            let error = format!("get album pictures error: {:?}", err);
            CommonResponse::failure(-1, error, vec![])
        }
    };
    Json(response)
}

#[derive(Deserialize)]
pub struct ForwardQuery {
    pub url: String
}

async fn forward_picture(Query(query): Query<ForwardQuery>, State(state): State<WebState>) -> Response {
    let response = match state.client.get(query.url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            error!("get picture error: {:?}", err);
            return (StatusCode::BAD_REQUEST, Body::empty()).into_response();
        }
    };

    if response.status().is_success() {
        let mut response_builder = Response::builder().status(response.status());
        *response_builder.headers_mut().unwrap() = response.headers().clone();
        response_builder.body(Body::from_stream(response.bytes_stream())).unwrap()
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, Body::empty()).into_response()
    }
}
