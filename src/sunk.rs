// use url;
use hyper::{self, Client, Uri};
use hyper_tls::HttpsConnector;
use json;
use log;
use tokio;

use api::Api;
use error::*;
use query::Query;

const SALT_SIZE: usize = 36; // Minimum 6 characters.

#[derive(Debug)]
pub struct Sunk {
    url:    Uri,
    auth:   SunkAuth,
    client: Client<HttpsConnector<hyper::client::HttpConnector>>,
    core:   tokio::reactor::Core,
    api:    Api,
}

#[derive(Debug)]
struct SunkAuth {
    user:     String,
    password: String,
}

impl SunkAuth {
    fn new(user: &str, password: &str) -> SunkAuth {
        SunkAuth {
            user:     user.into(),
            password: password.into(),
        }
    }

    // TODO Actual version comparison support
    fn as_uri(&self, api: Api) -> String {
        // First md5 support.
        let auth = if api >= "1.13.0".into() {
            use rand::{thread_rng, Rng};
            use md5;

            let salt: String =
                thread_rng().gen_ascii_chars().take(SALT_SIZE).collect();
            let pre_t = self.password.to_string() + &salt;
            let token = format!("{:x}", md5::compute(pre_t.as_bytes()));

            // As detailed in http://www.subsonic.org/pages/api.jsp
            format!("u={u}&t={t}&s={s}", u = self.user, t = token, s = salt)
        } else {
            format!("u={u}&p={p}", u = self.user, p = self.password)
        };

        // Prefer JSON.
        let format = if api >= "1.14.0".into() {
            "json"
        } else {
            "xml"
        };

        let crate_name = ::std::env::var("CARGO_PKG_NAME").unwrap();

        format!(
            "{auth}&v={v}&c={c}&f={f}",
            auth = auth,
            v = api,
            c = crate_name,
            f = format
        )
    }
}

impl Sunk {
    pub fn new(url: &str, user: &str, password: &str) -> Result<Sunk> {
        use std::str::FromStr;

        let auth = SunkAuth::new(user, password);
        let uri =
            Uri::from_str(url).map_err(|e| Error::Uri(UriError::Hyper(e)))?;
        let api = Api::from("1.14.0");

        let core = tokio::reactor::Core::new()?;
        let handle = core.handle();
        let client = Client::configure()
            .connector(HttpsConnector::new(4, &handle)
                .map_err(|_| Error::Other("Unable to use secure conection"))?)
            .build(&handle);

        Ok(Sunk {
            url:    uri,
            auth:   auth,
            client: client,
            core:   core,
            api:    api,
        })
    }

    /// Internal helper function to construct a URL when the actual fetching is
    /// not required.
    ///
    /// Formats arguments in a standard HTTP format, using information from the
    /// `Sunk`; for example:
    ///
    /// ```rust
    /// # use sunk::Sunk::*;
    /// # use error::*;
    ///
    /// let sunk = Sunk::new("subsonic.example.com", "user", "password")?;
    /// let url = sunk.build_url("stream", vec![("id", 1), ("bitrate", 96)])?;
    ///
    /// assert_eq!(
    ///     url,
    ///     "https://subsonic.example.com/rest/stream \
    ///         &u=user&p=password&v=1.14.0&id=1&bitrate=96".to_string()
    /// )
    /// ```
    ///
    /// Most usage of this function will be through `Sunk::get()`.
    #[allow(needless_pass_by_value)]
    fn build_url<'a, D>(&self, query: &str, args: Query<'a, D>) -> Result<String>
    where
        D: ::std::fmt::Display,
    {
        let scheme = self.url
            .scheme()
            .or_else(|| {
                warn!("No scheme provided; falling back to http");
                Some("http")
            })
            .ok_or_else(|| Error::Uri(UriError::Scheme))?;
        let addr = self.url
            .authority()
            .ok_or_else(|| Error::Uri(UriError::Address))?;

        let mut url = [scheme, "://", addr, "/rest/"].concat();
        url.push_str(query);
        url.push_str("?");
        url.push_str(&self.auth.as_uri(self.api));
        url.push_str("&");
        url.push_str(&args.to_string());

        Ok(url)
    }

    /// Issues a request to the `sunk` server.
    ///
    /// A query should be one documented in the [official API].
    ///
    /// [official API]: http://www.subsonic.org/pages/api.jsp
    ///
    /// # Errors
    ///
    /// Will return an error if any of the following occurs:
    ///
    /// - server is build with an incomplete URL
    /// - connecting to the server fails
    /// - the server returns an API error
    pub fn get<'a, D>(
        &mut self,
        query: &str,
        args: Query<'a, D>,
    ) -> Result<json::Value>
    where
        D: ::std::fmt::Display,
    {
        use futures::{Future, Stream};

        let uri = self.build_url(query, args)?.parse().unwrap();

        info!("Connecting to {}", uri);
        let work = self.client.get(uri).and_then(|res| {
            let status = res.status();
            info!("Received `{}` for request /{}?", status, query);

            res.body().concat2().and_then(move |body| {
                let v: json::Value = json::from_slice(&body).map_err(|e| {
                    use std::io;
                    io::Error::new(io::ErrorKind::Other, e)
                })?;
                Ok((status, v))
            })
        });

        let (status, res): (hyper::StatusCode, json::Value) =
            self.core.run(work)?;
        if status.is_success() {
            if let Some(out) =  res.get("subsonic-response") {
                println!("response: {}", out);
                println!("response0: {}", out[0]);
                println!("response0: {}", out[1]);
                println!("response2: {}", out[2]);
                match out["status"].as_str() {
                    Some("ok") => return Ok(out[2].clone()),
                    Some("failed") => {
                        return Err(Error::Api(ApiError::try_from(out)?))
                    }
                    _ => panic!()
                }
            } else {
                panic!()
            }
        } else {
            Err(Error::ConnectionError(status))
        }
    }

    /// Attempts to connect to the `Sunk` with the provided query and args.
    ///
    /// Returns the constructed, attempted URL on success, or an error if the
    /// Subsonic instance refuses the connection (i.e., returns a failure
    /// response).
    ///
    /// Specifically, it will succeed if `json::from_slice()` fails due to not
    /// receiving a valid JSON stream. It's assumed that the stream will be
    /// binary in this case.
    pub fn try_binary<'a, D>(
        &mut self,
        query: &str,
        args: Query<'a, D>,
    ) -> Result<String>
    where
        D: ::std::fmt::Display,
    {
        use futures::{Future, Stream};

        let raw_uri = self.build_url(query, args)?;
        let uri = raw_uri.parse().unwrap();

        info!("Connecting to {}", uri);
        let work = self.client.get(uri).and_then(|res| {
            res.body().concat2().and_then(move |b| {
                let valid_json = json::from_slice::<json::Value>(&b).is_ok();
                if !valid_json {
                    Ok(raw_uri)
                } else {
                    Err(hyper::Error::Method)
                }
            })
        });

        self.core.run(work).map_err(|e| Error::HyperError(e))
    }

    pub fn get_raw<'a, D>(
        &mut self,
        query: &str,
        args: Query<'a, D>
    ) -> Result<String>
    where
        D: ::std::fmt::Display,
    {
        use futures::{Future, Stream};

        let uri = self.build_url(query, args)?.parse().unwrap();

        info!("Connecting to {}", uri);
        let work = self.client.get(uri).and_then(|res| {
            res.body().concat2()
        });

        let get = self.core.run(work)?;
        String::from_utf8(get.to_vec())
            .map_err(|_| Error::Other("Unable to parse stream as UTF-8"))
    }

    /// Attempts to connect to the server with the provided credentials.
    fn check_connection(&mut self) -> Result<()> {
        self.get("ping", Query::with("", "")).map(|_| ())
    }

    /// Starts a library scan.
    pub fn scan_library(&mut self) -> Result<()> {
        self.get("startScan", Query::with("", ""))?;
        Ok(())
    }

    /// Gets the status of a scan. Returns whether or not the scan is currently
    /// running, and the number of media items found.
    pub fn scan_status(&mut self) -> Result<(bool, u64)> {
        let res = self.get("getScanStatus", Query::with("", ""))?;

        println!("{}", res);
        if let Some(status) = res["scanning"].as_bool() {
            if let Some(count) = res["count"].as_u64() {
                Ok((status, count))
            } else {
                unreachable!()
            }
        } else {
            unreachable!()
        }

    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use sunk::*;
    use test_util::*;

    #[test]
    fn remote_try_binary() {
        let (site, user, pass) = load_credentials().unwrap();
        let mut srv = Sunk::new(&site, &user, &pass).unwrap();
        let resp = srv.try_binary("stream", Query::with("id", 1));
        assert!(resp.is_ok())
    }

    #[test]
    fn remote_ping() {
        let (site, user, pass) = load_credentials().unwrap();
        let mut srv = Sunk::new(&site, &user, &pass).unwrap();
        debug!("{:?}", srv);
        srv.check_connection().unwrap();
        assert!(srv.check_connection().is_ok())
    }

    #[test]
    fn remote_scan_status() {
        let (site, user, pass) = load_credentials().unwrap();
        let mut srv = Sunk::new(&site, &user, &pass).unwrap();
        let (status, n) = srv.scan_status().unwrap();
        assert_eq!(status, false);
        assert_eq!(n, 5661);
    }
}
