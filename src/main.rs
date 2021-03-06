use pulldown_cmark::{Parser, Event, Tag};
use std::fs;
use futures::future::{select_all, BoxFuture, FutureExt};
use std::collections::{BTreeSet, BTreeMap};
use serde::{Serialize, Deserialize};
use lazy_static::lazy_static;
use std::sync::atomic::{AtomicU32, Ordering};
use async_std::task;
use std::time;
use log::{warn, debug};
use std::io::Write;
use reqwest::{Client, redirect::Policy, StatusCode, header};
use regex::Regex;
use scraper::{Html, Selector};
use failure::{Fail, Error, format_err};

#[derive(Debug, Fail)]
enum CheckerError {
    #[fail(display = "failed to try url")]
    NotTried, // Generally shouldn't happen, but useful to have

    #[fail(display = "http error: {}", status)]
    HttpError {
        status: StatusCode,
        location: Option<String>,
    },

    #[fail(display = "reqwest error: {}", error)]
    ReqwestError {
        error: reqwest::Error,
    }
}

struct MaxHandles {
    remaining: AtomicU32
}

struct Handle<'a> {
    parent: &'a MaxHandles
}

impl MaxHandles {
    fn new(max: u32) -> MaxHandles {
        MaxHandles { remaining: AtomicU32::new(max) }
    }

    async fn get<'a>(&'a self) -> Handle<'a> {
        loop {
            let current = self.remaining.load(Ordering::Relaxed);
            if current > 0 {
                let new_current = self.remaining.compare_and_swap(current, current - 1, Ordering::Relaxed);
                if new_current == current { // worked
                    debug!("Got handle with {}", new_current);
                    return Handle { parent: self };
                }
            }
            task::sleep(time::Duration::from_millis(500)).await;
        }
    }
}

impl<'a> Drop for Handle<'a> {
    fn drop(&mut self) {
        debug!("Dropping");
        self.parent.remaining.fetch_add(1, Ordering::Relaxed);
    }
}

lazy_static! {
    static ref CLIENT: Client = Client::builder()
        .danger_accept_invalid_certs(true) // because some certs are out of date
        .user_agent("curl/7.54.0") // so some sites (e.g. sciter.com) don't reject us
        .redirect(Policy::none())
        .timeout(time::Duration::from_secs(20))
        .build().unwrap();

    // This is to avoid errors with running out of file handles, so we only do 20 requests at a time
    static ref HANDLES: MaxHandles = MaxHandles::new(20);
}

fn get_url(url: String) -> BoxFuture<'static, (String, Result<String, CheckerError>)> {
    async move {
        let _handle = HANDLES.get().await;
        let mut res = Err(CheckerError::NotTried);
        for _ in 0..5u8 {
            debug!("Running {}", url);
            let resp = CLIENT
                .get(&url)
                .header(header::ACCEPT, "text/html, */*;q=0.8")
                .send()
                .await;
            match resp {
                Err(err) => {
                    warn!("Error while getting {}, retrying: {}", url, err);
                    res = Err(CheckerError::ReqwestError{error: err});
                    continue;
                }
                Ok(ref ok) => {
                    let status = ok.status();
                    if status != StatusCode::OK {
                        lazy_static! {
                            static ref ACTIONS_REGEX: Regex = Regex::new(r"https://github.com/(?P<org>[^/]+)/(?P<repo>[^/]+)/actions(?:\?workflow=.+)?").unwrap();
                        }
                        if status == StatusCode::NOT_FOUND && ACTIONS_REGEX.is_match(&url) {
                            let rewritten = ACTIONS_REGEX.replace_all(&url, "https://github.com/$org/$repo");
                            warn!("Got 404 with Github actions, so replacing {} with {}", url, rewritten);
                            let (_new_url, res) = get_url(rewritten.to_string()).await;
                            return (url, res);
                        }

                        warn!("Error while getting {}, retrying: {}", url, status);
                        if status.is_redirection() {
                            res = Err(CheckerError::HttpError {status: status, location: ok.headers().get(header::LOCATION).and_then(|h| h.to_str().ok()).map(|x| x.to_string())});
                        } else {
                            res = Err(CheckerError::HttpError {status: status, location: None});
                        }
                        continue;
                    }
                    debug!("Finished {}", url);
                    res = Ok(format!("{:?}", ok));
                    break;
                }
            }
        }
        (url, res)
    }.boxed()
}

#[derive(Debug, Serialize, Deserialize)]
struct Results {
    working: BTreeSet<String>,
    failed: BTreeMap<String, String>
}

impl Results {
    fn new() -> Results {
        Results {
            working: BTreeSet::new(),
            failed: BTreeMap::new()
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::init();
    let markdown_input = fs::read_to_string("README.md").expect("Can't read README.md");
    let parser = Parser::new(&markdown_input);

    let mut results: Results = fs::read_to_string("results.yaml")
        .map_err(|e| format_err!("{}", e))
        .and_then(|x| serde_yaml::from_str(&x).map_err(|e| format_err!("{}", e)))
        .unwrap_or(Results::new());
    results.failed.clear();

    let mut url_checks = vec![];

    let mut do_check = |url: String| {
        if !url.starts_with("http") {
            return;
        }
        if results.working.contains(&url) {
            return;
        }
        let check = get_url(url).boxed();
        url_checks.push(check);
    };

    for (event, _range) in parser.into_offset_iter() {
        match event {
            Event::Start(tag) => {
                match tag {
                    Tag::Link(_link_type, url, _title) | Tag::Image(_link_type, url, _title) => {
                        do_check(url.to_string());
                    }
                    _ => {}
                }
            }
            Event::Html(content) => {
                let fragment = Html::parse_fragment(&content);
                for element in fragment.select(&Selector::parse("img").unwrap()) {
                    let img_src = element.value().attr("src");
                    if let Some(src) = img_src {
                        do_check(src.to_string());
                    }
                }
                for element in fragment.select(&Selector::parse("a").unwrap()) {
                    let a_href = element.value().attr("href");
                    if let Some(href) = a_href {
                        do_check(href.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    while url_checks.len() > 0 {
        debug!("Waiting...");
        let ((url, res), _index, remaining) = select_all(url_checks).await;
        url_checks = remaining;
        match res {
            Ok(_) => {
                print!("\u{2714} ");
                results.working.insert(url);
            },
            Err(err) => {
                print!("\u{2718} ");
                let message = match err {
                    CheckerError::HttpError {status, location} => {
                        match location {
                            Some(loc) => {
                                format!("[{}] {} -> {}", status.as_u16(), url, loc)
                            }
                            None => {
                                format!("[{}] {}", status.as_u16(), url)
                            }
                        }
                    }
                    _ => {
                        format!("{:?}", err)
                    }
                };
                results.failed.insert(url, message);
            }
        }
        std::io::stdout().flush().unwrap();
        fs::write("results.yaml", serde_yaml::to_string(&results)?)?;
    }
    println!("");
    if results.failed.is_empty() {
        println!("No errors!");
        Ok(())
    } else {
        for (_url, error) in &results.failed {
            println!("{}", error);
        }
        Err(format_err!("{} urls with errors", results.failed.len()))
    }
}