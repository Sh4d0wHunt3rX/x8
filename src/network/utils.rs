use std::{error::Error, time::Duration};

use lazy_static::lazy_static;
use percent_encoding::{AsciiSet, CONTROLS};
use regex::Regex;
use reqwest::Client;
use serde::Serialize;

use crate::{config::structs::Config, utils::random_line};

use super::response::Response;

lazy_static! {
    /// characters to encode in case --encode option provided
    pub static ref FRAGMENT: AsciiSet = CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'<')
        .add(b'>')
        .add(b'`')
        .add(b'&')
        .add(b'#')
        .add(b';')
        .add(b'/')
        .add(b'=')
        .add(b'%');
}

/// enum mainly created for the correct json parsing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    /// we need a different data type for json because some json values can be used without quotes (numbers, booleans, ..)
    /// and therefore this type should be treated differently
    Json,

    /// that's from parsed request's content-type header
    /// needs to be ignored in case the injection points not within the body
    /// to exclude false positive /?{"ZXxZPLN":"ons9XDZ", ..} or Cookie: {"ZXxZPLN":"ons9XDZ", ..} queries
    // it still can be bypassed with the correct --data-type argument
    ProbablyJson,

    Urlencoded,
    Headers,
}

/// where to insert parameters
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Copy)]
pub enum InjectionPlace {
    Path,
    Body,
    Headers,
    HeaderValue,
}

impl Default for InjectionPlace {
    fn default() -> Self {
        InjectionPlace::Path
    }
}

pub trait Headers {
    fn contains_key(&self, key: &str) -> bool;
    fn get_index_case_insensitive(&self, key: &str) -> Option<usize>;
    fn get_value(&self, key: &str) -> Option<String>;
    fn get_value_case_insensitive(&self, key: &str) -> Option<String>;
}

impl Headers for Vec<(String, String)> {
    fn contains_key(&self, key: &str) -> bool {
        for (k, _) in self.iter() {
            if k == key {
                return true;
            }
        }
        false
    }

    fn get_index_case_insensitive(&self, key: &str) -> Option<usize> {
        self.iter()
            .position(|r| r.0.to_lowercase() == key.to_ascii_lowercase())
    }

    fn get_value(&self, key: &str) -> Option<String> {
        for (k, v) in self.iter() {
            if k == key {
                return Some(v.to_owned());
            }
        }
        None
    }

    fn get_value_case_insensitive(&self, key: &str) -> Option<String> {
        let key = key.to_lowercase();
        for (k, v) in self.iter() {
            if k.to_lowercase() == key {
                return Some(v.to_owned());
            }
        }
        None
    }
}

/// writes request and response to a file
/// return file location
pub(super) fn save_request(
    config: &Config,
    response: &Response,
    param_key: &str,
) -> Result<String, Box<dyn Error>> {
    let output = response.print_all();

    let filename = format!(
        "{}/{}-{}-{}-{}",
        &config.save_responses,
        &response.request.as_ref().unwrap().defaults.host,
        response
            .request
            .as_ref()
            .unwrap()
            .defaults
            .method
            .to_lowercase(),
        param_key,
        random_line(3) //nonce to prevent overwrites
    );

    std::fs::write(&filename, output)?;

    Ok(filename)
}

pub fn create_client(config: &Config, replay: bool) -> Result<Client, Box<dyn Error>> {
    let mut client = Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(config.timeout as u64))
        .http1_title_case_headers()
        .cookie_store(true)
        .http09_responses()
        .use_rustls_tls();

    if config.disable_trustdns {
        client = client.no_trust_dns();
    }

    if replay {
        client = client.proxy(match reqwest::Proxy::all(&config.replay_proxy) {
            Ok(val) => val,
            Err(err) => Err(format!("Unable to parse replay_proxy: {}", err))?,
        });
    } else {
        if !config.proxy.is_empty() {
            client = client.proxy(reqwest::Proxy::all(&config.proxy)?);
        }
    }

    if !config.follow_redirects {
        client = client.redirect(reqwest::redirect::Policy::none());
    }

    if config.http_version.is_some() {
        match config.http_version {
            Some(http::Version::HTTP_11) => client = client.http1_only(),
            Some(http::Version::HTTP_2) => client = client.http2_prior_knowledge(),
            _ => unreachable!(),
        }
    }

    Ok(client.build()?)
}

/// check whether the content is binary
/// so we can ignore the body in comparing
/// a few reasons for it:
/// 1. the comparing of binary content takes a lot of time
/// 2. page diff anyway will be checked by the content-length header
/// because the content-length header usually static for binary files
pub fn is_binary_content(content_type: Option<String>) -> bool {
    lazy_static! {
        static ref RE_BINARY_MIME: Regex = Regex::new(
            "((video|audio|font|image)/\
    |\
    /(zip|octet-stream|x-tar|vnd\\.rar|pdf|gzip|epub-zip|x-bzip|x-bzip2|x-freearc|x-7z-compressed))"
        ).unwrap();
    }

    content_type.is_some() && RE_BINARY_MIME.is_match(&content_type.unwrap())
}
