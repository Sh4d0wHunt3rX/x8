use crate::{config::structs::Config, utils::random_line, RANDOM_LENGTH, VALUE_LENGTH};
use itertools::Itertools;
use lazy_static::lazy_static;
use percent_encoding::utf8_percent_encode;
use regex::Regex;
use reqwest::Client;
use std::{
    collections::HashMap,
    convert::TryFrom,
    error::Error,
    iter::FromIterator,
    time::{Duration, Instant},
};
use url::Url;

/// in order to be able to use make_query() for headers as well
const HEADERS_TEMPLATE: &str = "%k\x00@%=%@\x00%v";
const HEADERS_MIDDLE: &str = "\x00@%=%@\x00";
const HEADERS_JOINER: &str = "\x01@%&%@\x01";

use super::{
    response::Response,
    utils::{create_client, is_binary_content, DataType, Headers, InjectionPlace, FRAGMENT},
};

#[derive(Debug, Clone, Default)]
pub struct RequestDefaults {
    /// default request data
    pub method: String,
    pub scheme: String,
    pub path: String,
    pub host: String,
    pub port: u16,

    /// custom user supplied headers or default ones
    pub custom_headers: Vec<(String, String)>,

    /// how much to sleep between requests in millisecs
    pub delay: Duration, //MOVE to config

    /// default reqwest client
    pub client: Client,

    /// parameter template, for example %k=%v
    pub template: String,

    /// how to join parameters, for example '&'
    pub joiner: String,

    /// whether to encode the query like param1=value1&param2=value2 -> param1%3dvalue1%26param2%3dvalue2
    pub encode: bool,

    /// to replace {"key": "false"} with {"key": false}
    pub is_json: bool,

    /// default body
    pub body: String,

    /// whether to include parameters like debug=true to the list
    pub disable_custom_parameters: bool,

    /// parameters to add to every request
    /// it is used in recursion search
    pub parameters: Vec<(String, String)>,

    /// where the injection point is
    pub injection_place: InjectionPlace,

    /// the default amount of reflection per non existing parameter
    pub amount_of_reflections: usize,

    /// check body of responses with binary content type
    pub check_binary: bool,
}

#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub defaults: &'a RequestDefaults,

    /// vector of supplied parameters
    pub parameters: Vec<String>,

    /// parsed parameters (key, value)
    pub prepared_parameters: Vec<(String, String)>,

    /// parameters with not random values
    /// we need this vector to ignore searching for reflections for these parameters
    /// for example admin=1 - its obvious that 1 can be reflected unpredictable amount of times
    pub non_random_parameters: Vec<(String, String)>,

    pub headers: Vec<(String, String)>,

    pub body: String,

    /// we can't use defaults.path because there can be {{random}} variable that need to be replaced
    pub path: String,

    /// whether the request was prepared
    /// {{random}} things replaced, prepared_parameters filled
    pub prepared: bool,
}

impl<'a> Request<'a> {
    pub fn new(l: &'a RequestDefaults, parameters: Vec<String>) -> Self {
        Self {
            path: l.path.to_owned(),
            defaults: l,
            headers: Vec::new(),
            body: l.body.clone(),
            parameters,
            prepared_parameters: Vec::new(), //l.parameters.clone(),
            non_random_parameters: Vec::new(),
            prepared: false,
        }
    }

    pub fn new_random(l: &'a RequestDefaults, max: usize) -> Self {
        let parameters = Vec::from_iter((0..max).map(|_| random_line(VALUE_LENGTH)));
        Request::new(l, parameters)
    }

    pub fn set_header<S: Into<String>>(&mut self, key: S, value: S) {
        self.headers.push((key.into(), value.into()));
    }

    pub fn set_headers(&mut self, headers: Vec<(String, String)>) {
        for (k, v) in headers {
            self.headers.push((k, v));
        }
    }

    pub fn url(&self) -> String {
        format!(
            "{}://{}:{}{}",
            &self.defaults.scheme, &self.defaults.host, &self.defaults.port, &self.path
        )
    }

    pub fn make_query(&self) -> String {
        lazy_static! {
            static ref RE_JSON_WORDS_WITHOUT_QUOTES: Regex =
                Regex::new(r#"^([1-9]\d*|null|false|true)$"#).unwrap();
        }

        let query = if self.defaults.is_json {
            self.prepared_parameters
                .iter()
                .chain(self.defaults.parameters.iter())
                // not very optimal because we know that there's a lot of random parameters
                // that doesn't need to be checked
                .map(|(k, v)| {
                    if RE_JSON_WORDS_WITHOUT_QUOTES.is_match(v) {
                        self.defaults.template.replace("%k", k).replace("%v", v)
                    } else {
                        self.defaults
                            .template
                            .replace("%k", k)
                            .replace("%v", &format!("\"{}\"", v))
                    }
                })
                .collect::<Vec<String>>()
                .join(&self.defaults.joiner)
        } else {
            self.prepared_parameters
                .iter()
                .chain(self.defaults.parameters.iter())
                .map(|(k, v)| self.defaults.template.replace("%k", k).replace("%v", v))
                .collect::<Vec<String>>()
                .join(&self.defaults.joiner)
        };

        if self.defaults.encode {
            utf8_percent_encode(&query, &FRAGMENT).to_string()
        } else {
            query
        }
    }

    /// replace injection points with parameters
    /// replace templates ({{random}}) with random values
    /// additional param is for reflection counting TODO REMOVE
    ///
    /// in case self.parameters contains parameter with "="
    /// it gets splitted by =  and the default random value gets replaced with the right part:
    /// admin=true -> (admin, true) vs admin -> (admin, df32w)
    pub fn prepare(&mut self) {
        if self.prepared {
            return;
        }
        self.prepared = true;

        self.non_random_parameters = Vec::from_iter(
            self.parameters
                .iter()
                .filter(|x| x.contains('='))
                .map(|x| x.split('='))
                .map(|mut x| {
                    (
                        x.next().unwrap().to_owned(),
                        x.next().unwrap_or("").to_owned(),
                    )
                }),
        );

        self.prepared_parameters = Vec::from_iter(
            // append self.prepared_parameters (can be set from RequestDefaults using recursive search)
            self.prepared_parameters
                .iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                // append parameters with not random values
                .chain(
                    self.non_random_parameters
                        .iter()
                        .map(|(k, v)| (k.to_owned(), v.to_owned())),
                )
                // append random parameters
                .chain(
                    self.parameters
                        .iter()
                        .filter(|x| !x.is_empty() && !x.contains("="))
                        .map(|x| (x.to_owned(), random_line(VALUE_LENGTH))),
                ),
        );

        if self.defaults.injection_place != InjectionPlace::HeaderValue {
            for (k, v) in self.defaults.custom_headers.iter() {
                self.set_header(k, &v.replace("{{random}}", &random_line(RANDOM_LENGTH)));
            }
        }
        self.path = self.path.replace("{{random}}", &random_line(RANDOM_LENGTH));
        self.body = self.body.replace("{{random}}", &random_line(RANDOM_LENGTH));

        match self.defaults.injection_place {
            InjectionPlace::Path => self.path = self.path.replace("%s", &self.make_query()),
            InjectionPlace::Body => {
                self.body = self.body.replace("%s", &self.make_query());

                if !self.defaults.custom_headers.contains_key("Content-Type") {
                    if self.defaults.is_json {
                        self.set_header("Content-Type", "application/json");
                    } else {
                        self.set_header("Content-Type", "application/x-www-form-urlencoded");
                    }
                }
            }
            InjectionPlace::HeaderValue => {
                // in case someone searches headers while sending a valid body - it's usually important to set Content-Type header as well.
                if !self.defaults.custom_headers.contains_key("Content-Type")
                    && self.defaults.method != "GET"
                    && self.defaults.method != "HEAD"
                    && !self.body.is_empty()
                {
                    if self.body.starts_with('{') {
                        self.set_header("Content-Type", "application/json");
                    } else {
                        self.set_header("Content-Type", "application/x-www-form-urlencoded");
                    }
                }

                for (k, v) in self.defaults.custom_headers.iter() {
                    self.set_header(
                        k,
                        &v.replace("{{random}}", &random_line(RANDOM_LENGTH))
                            .replace("%s", &self.make_query()),
                    );
                }
            }
            InjectionPlace::Headers => {
                // in case someone searches headers while sending a valid body - it's usually important to set Content-Type header as well.
                if !self.defaults.custom_headers.contains_key("Content-Type")
                    && self.defaults.method != "GET"
                    && self.defaults.method != "HEAD"
                    && !self.body.is_empty()
                {
                    if self.body.starts_with('{') {
                        self.set_header("Content-Type", "application/json");
                    } else {
                        self.set_header("Content-Type", "application/x-www-form-urlencoded");
                    }
                }

                let headers: Vec<(String, String)> = self
                    .make_query()
                    .split(&self.defaults.joiner)
                    .filter(|x| !x.is_empty())
                    .map(|x| x.split(HEADERS_MIDDLE))
                    .map(|mut x| (x.next().unwrap().to_owned(), x.next().unwrap().to_owned()))
                    .collect();

                self.set_headers(headers);
            }
        }
    }

    pub async fn send_by(self, clients: &Client) -> Result<Response<'a>, Box<dyn Error>> {
        match self.clone().request(clients).await {
            Ok(val) => Ok(val),
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(self.clone().request(clients).await?)
            }
        }
    }

    // we need to somehow impl Send and Sync for error (for using send() within async recursive func)
    // therefore we are wrapping the original call to send()
    // not a good way tho, maybe someone can suggest a better one
    pub async fn wrapped_send(self) -> Result<Response<'a>, Box<dyn Error + Send + Sync>> {
        match self.send().await {
            Err(err) => Err(err.to_string().into()),
            Ok(val) => Ok(val),
        }
    }

    pub async fn send(self) -> Result<Response<'a>, Box<dyn Error>> {
        let dc = &self.defaults.client;
        self.send_by(dc).await
    }

    async fn request(mut self, client: &Client) -> Result<Response<'a>, reqwest::Error> {
        self.prepare();

        let mut request = http::Request::builder()
            .method(self.defaults.method.as_str())
            .uri(self.url());

        for (k, v) in &self.headers {
            request = request.header(k, v)
        }

        let request = request.body(self.body.to_owned()).unwrap();

        tokio::time::sleep(self.defaults.delay).await;

        let reqwest_req = reqwest::Request::try_from(request).unwrap();

        let start = Instant::now();

        let res = client.execute(reqwest_req).await?;

        let duration = start.elapsed();

        let mut headers: Vec<(String, String)> = Vec::new();

        for (k, v) in res.headers() {
            let k = k.to_string();

            // sometimes conversion may fail
            let v = match v.to_str() {
                Ok(val) => val,
                Err(_) => {
                    log::debug!("Unable to parse {} header. The value is {:?}", k, v);
                    ""
                }
            }
            .to_string();

            headers.push((k, v));
        }

        let code = res.status().as_u16();
        let http_version = Some(res.version());

        let body_bytes = res.bytes().await?.to_vec();

        let text = if is_binary_content(headers.get_value_case_insensitive("content-type"))
            && !self.defaults.check_binary
        {
            String::new()
        } else {
            String::from_utf8_lossy(&body_bytes).to_string()
        };

        let mut response = Response {
            code,
            headers,
            time: duration.as_millis(),
            text,
            request: Some(self),
            reflected_parameters: HashMap::new(),
            http_version,
        };

        response.beautify_body();
        response.add_headers();

        Ok(response)
    }

    /// the function is used when there was a error during the request
    pub fn empty_response(mut self) -> Response<'a> {
        self.prepare();
        Response {
            time: 0,
            code: 0,
            headers: Vec::new(),
            text: String::new(),
            reflected_parameters: HashMap::new(),
            request: Some(self),
            http_version: None,
        }
    }

    pub fn print(&mut self) -> String {
        self.prepare();
        self.print_sent()
    }

    pub fn print_sent(&self) -> String {
        let host = if self.headers.contains_key("Host") {
            self.headers.get_value("Host").unwrap()
        } else {
            self.defaults.host.to_owned()
        };

        let mut str_req = format!(
            "{} {} HTTP/1.1\nHost: {}\n",
            &self.defaults.method, self.path, host
        );

        for (k, v) in self.headers.iter().sorted() {
            if k != "Host" {
                str_req += &format!("{}: {}\n", k, v)
            }
        }

        str_req += &format!("\n{}", self.body);

        str_req
    }
}

impl<'a> RequestDefaults {
    pub fn from_config<S: Into<String>>(
        config: &Config,
        method: S,
        url: S,
    ) -> Result<Self, Box<dyn Error>> {
        Self::new(
            method.into().as_str(), //method needs to be set explicitly via .set_method()
            url.into().as_str(),    //as well as url
            config.custom_headers.clone(),
            config.delay,
            create_client(config, false)?,
            config.template.clone(),
            config.joiner.clone(),
            config.encode,
            config.data_type.clone(),
            config.invert,
            config.headers_discovery,
            &config.body,
            config.disable_custom_parameters,
            config.check_binary,
        )
    }

    pub fn new<S: Into<String> + From<String> + std::fmt::Debug>(
        method: &str,
        url: &str,
        custom_headers: Vec<(String, String)>,
        delay: Duration,
        client: Client,
        template: Option<S>,
        joiner: Option<S>,
        encode: bool,
        mut data_type: Option<DataType>,
        invert: bool,
        headers_discovery: bool,
        body: &str,
        disable_custom_parameters: bool,
        check_binary: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let mut injection_place = if headers_discovery {
            InjectionPlace::Headers
        } else if (method == "POST" || method == "PUT" || method == "PATCH" || method == "DELETE")
            && !invert
            || (method != "POST"
                && method != "PUT"
                && method != "PATCH"
                && method != "DELETE"
                && invert)
        {
            InjectionPlace::Body
        } else {
            InjectionPlace::Path
        };

        if headers_discovery {
            data_type = Some(DataType::Headers);

            if custom_headers.iter().any(|x| x.1.contains("%s")) {
                injection_place = InjectionPlace::HeaderValue;
            }
        }

        let data_type = if data_type != Some(DataType::ProbablyJson) {
            data_type

        // explained in DataType enum comments
        // tl.dr. data_type was taken from a parsed request's content-type so we are not 100% sure what did a user mean
        // we don't need probablyurlencoded because urlencoded is fine for get requests
        } else if injection_place == InjectionPlace::Body
            && data_type == Some(DataType::ProbablyJson)
        {
            Some(DataType::Json)
        } else if injection_place == InjectionPlace::Path {
            Some(DataType::Urlencoded)
        } else {
            unreachable!()
        };

        let (guessed_template, guessed_joiner, is_json, data_type) =
            RequestDefaults::guess_data_format(body, &injection_place, data_type);

        let (template, joiner) = (
            template
                .unwrap_or_else(|| guessed_template.to_string().into())
                .into(),
            joiner
                .unwrap_or_else(|| guessed_joiner.to_string().into())
                .into()
                .replace("\\r", "\r")
                .replace("\\n", "\n"),
        );

        let url = Url::parse(url)?;

        let (path, body) = if let Some(data_type) = data_type {
            RequestDefaults::fix_path_and_body(
                // &url[url::Position::BeforePath..].to_string() instead of url.path() because we need to preserve query as well
                &url[url::Position::BeforePath..],
                body,
                &joiner,
                &injection_place,
                data_type,
            )
        } else {
            // injection within headers
            (
                url[url::Position::BeforePath..].to_string(),
                body.to_owned(),
            )
        };

        Ok(Self {
            method: method.to_string(),
            scheme: url.scheme().to_string(),
            path,
            host: url.host().ok_or("Host missing")?.to_string(),
            custom_headers,
            port: url.port_or_known_default().ok_or("Wrong scheme")?,
            delay,
            client,
            template,
            joiner,
            encode,
            is_json,
            body,
            disable_custom_parameters,
            injection_place,

            amount_of_reflections: 0,

            parameters: Vec::new(),

            check_binary,
        })
    }

    /// returns template, joiner, whether the data is json, DataType if the injection point isn't within headers
    fn guess_data_format(
        body: &str,
        injection_place: &InjectionPlace,
        data_type: Option<DataType>,
    ) -> (&'a str, &'a str, bool, Option<DataType>) {
        if data_type.is_some() && data_type != Some(DataType::Headers) {
            match data_type {
                // %v isn't within quotes because not every json value needs to be in quotes
                Some(DataType::Json) => ("\"%k\":%v", ",", true, Some(DataType::Json)),
                Some(DataType::Urlencoded) => ("%k=%v", "&", false, Some(DataType::Urlencoded)),
                _ => unreachable!(),
            }
        } else {
            match injection_place {
                InjectionPlace::Body => {
                    if body.starts_with('{') {
                        ("\"%k\":%v", ",", true, Some(DataType::Json))
                    } else {
                        ("%k=%v", "&", false, Some(DataType::Urlencoded))
                    }
                }
                InjectionPlace::HeaderValue => ("%k=%v", ";", false, None),
                InjectionPlace::Path => ("%k=%v", "&", false, Some(DataType::Urlencoded)),
                InjectionPlace::Headers => (HEADERS_TEMPLATE, HEADERS_JOINER, false, None),
            }
        }
    }

    /// adds injection points where necessary
    fn fix_path_and_body(
        path: &str,
        body: &str,
        joiner: &str,
        injection_place: &InjectionPlace,
        data_type: DataType,
    ) -> (String, String) {
        match injection_place {
            InjectionPlace::Body => {
                if body.contains("%s") {
                    (path.to_string(), body.to_string())
                } else if body.is_empty() {
                    match data_type {
                        DataType::Urlencoded => (path.to_string(), "%s".to_string()),
                        DataType::Json => (path.to_string(), "{%s}".to_string()),
                        _ => unreachable!(),
                    }
                } else {
                    match data_type {
                        DataType::Urlencoded => (path.to_string(), format!("{}{}%s", body, joiner)),
                        DataType::Json => {
                            let mut body = body.to_owned();
                            body.pop(); // remove the last '}'
                            if body != "{" {
                                (path.to_string(), format!("{},%s}}", body))
                            } else {
                                // the json body was empty so the first comma is not needed
                                (path.to_string(), format!("{}%s}}", body))
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
            InjectionPlace::Path => {
                if path.contains("%s") {
                    (path.to_string(), body.to_string())
                } else if path.contains('?') {
                    (format!("{}{}%s", path, joiner), body.to_string())
                } else if joiner == "&" {
                    (format!("{}?%s", path), body.to_string())
                } else {
                    // some very non-standart configuration
                    (format!("{}%s", path), body.to_string())
                }
            }
            _ => (path.to_string(), body.to_string()),
        }
    }

    /// recreates url
    pub fn url(&self) -> String {
        format!("{}://{}:{}{}", self.scheme, self.host, self.port, self.path)
    }

    /// recreates url without default port
    pub fn url_without_default_port(&self) -> String {
        let port = if self.port == 443 || self.port == 80 {
            String::new()
        } else {
            format!(":{}", self.port)
        };

        format!("{}://{}{}{}", self.scheme, self.host, port, self.path)
    }
}
